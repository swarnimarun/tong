//! `Tong.toml` native targets → Rust model.
//!
//! Loads the whole workspace: the root manifest's targets (the root
//! package) plus every member manifest matched by the `[workspace]
//! members`/`default_members` globs. Members inherit the root
//! toolchain/profiles/store/registry/policy; each member's targets lower
//! into packages grouped by `(member path, package_name, version)`.
//! Cargo-style auto-detection (src/lib.rs, src/main.rs) applies so native
//! manifests stay small.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use tong_graph::manifest::{Lto, Manifest, OptLevel, ProfileConfig, TargetConfig, TargetDepConfig};
use tong_rust::model::{
    BinTarget, CcImport, Dep, Edition, LibTarget, Package, PackageId, ProfileSpec, RustModel,
    SourceId, TestTarget, source_rel_path,
};

/// The supported Rust rules, for the unknown-rule diagnostic.
const SUPPORTED_RULES: &[&str] = &[
    "rust_binary",
    "rust_library",
    "rust_proc_macro",
    "rust_test",
    "rust_example",
    "rust_bench",
    "rust_doc_test",
    "cc_import",
];

/// Converts a native `Tong.toml` workspace into the Rust model.
pub fn manifest_to_model(manifest: &Manifest, root: &Path) -> Result<RustModel, String> {
    let mut model = RustModel {
        resolver: tong_rust::ResolverVersion::V2,
        ..Default::default()
    };

    // The root manifest's targets form the root package; every matched
    // member directory contributes its own package(s). All directory
    // identity is canonical (symlink-safe); the workspace-relative path is
    // derived separately for package sources.
    let root_canonical = fs::canonicalize(root).map_err(|err| {
        format!(
            "cannot canonicalize the workspace root {}: {err}",
            root.display()
        )
    })?;
    let mut dirs: Vec<PathBuf> = vec![root_canonical.clone()];
    dirs.extend(expand_members(root, &manifest.workspace.members)?);

    // Load every member manifest first (held in a Vec so borrowed target
    // refs outlive the loop; the root manifest is the caller's).
    let mut member_manifests: Vec<Manifest> = Vec::new();
    for dir in dirs.iter().skip(1) {
        member_manifests.push(Manifest::load(dir).map_err(|err| err.to_string())?);
    }

    // label → (member dir, target) for dependency resolution.
    let mut targets_by_label: BTreeMap<(PathBuf, String), &TargetConfig> = BTreeMap::new();
    for (dir, member_manifest) in dirs
        .iter()
        .zip(std::iter::once(manifest).chain(member_manifests.iter()))
    {
        for (key, target) in &member_manifest.target {
            targets_by_label.insert((dir.clone(), key.clone()), target);
        }
    }

    let mut packages: BTreeMap<(PathBuf, String, String), PackageSeed> = BTreeMap::new();
    let mut cc_imports: Vec<CcImport> = Vec::new();
    for dir in &dirs {
        let member_manifest = if dir == &root_canonical {
            manifest
        } else {
            &Manifest::load(dir).map_err(|err| err.to_string())?
        };
        let rel = source_rel_path(dir, &root_canonical);
        for (key, target) in &member_manifest.target {
            if target.rule == "cc_import" {
                cc_imports.push(resolve_cc_import(key, target, dir)?);
                continue;
            }
            let package_name = target.package_name.clone().unwrap_or_else(|| key.clone());
            let version = target.version.clone().unwrap_or_else(|| "0.0.0".to_owned());
            let seed = packages
                .entry((dir.clone(), package_name.clone(), version.clone()))
                .or_insert_with(|| PackageSeed {
                    dir: dir.clone(),
                    package_name: package_name.clone(),
                    version: version.clone(),
                    rel: rel.clone(),
                    edition: None,
                    lib: None,
                    bins: Vec::new(),
                    tests: Vec::new(),
                    build_script: None,
                    deps: Vec::new(),
                    dev_deps: Vec::new(),
                    features: BTreeMap::new(),
                    has_default_feature: false,
                    rustflags: Vec::new(),
                    env: BTreeMap::new(),
                });
            seed.add_target(key, target, dir, root, &targets_by_label)?;
        }
    }

    for (_, seed) in packages {
        model.packages.push(seed.into_package()?);
    }
    model.members = model.packages.iter().map(|pkg| pkg.id.clone()).collect();
    // `[workspace] default_members` narrows the default build selection.
    if !manifest.workspace.default_members.is_empty() {
        let defaults = expand_members(root, &manifest.workspace.default_members)?;
        let default_dirs: Vec<PathBuf> = defaults
            .iter()
            .map(|dir| fs::canonicalize(dir).unwrap_or_else(|_| dir.clone()))
            .collect();
        let root_canonical = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        model.default_members = model
            .packages
            .iter()
            .filter(|pkg| {
                let canonical = fs::canonicalize(&pkg.dir).unwrap_or_else(|_| pkg.dir.clone());
                default_dirs.contains(&canonical) || canonical == root_canonical
            })
            .map(|pkg| pkg.id.clone())
            .collect();
    }
    model.cc_imports = cc_imports;

    model.profiles = manifest
        .profile
        .iter()
        .map(|(name, config)| (name.clone(), profile_from_config(config)))
        .collect();
    model
        .profiles
        .entry("dev".to_owned())
        .or_insert_with(ProfileSpec::dev);
    model
        .profiles
        .entry("release".to_owned())
        .or_insert_with(ProfileSpec::release);

    Ok(model)
}

/// Expands workspace member entries (globs like `crates/*` or exact
/// relative directories) into canonical sorted directories, each holding a
/// `Tong.toml`. Unmatched globs and duplicate canonical directories are
/// errors.
fn expand_members(root: &Path, members: &[String]) -> Result<Vec<PathBuf>, String> {
    let root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let mut out: Vec<PathBuf> = Vec::new();
    for member in members {
        if let Some(glob) = member.strip_suffix("/*") {
            let dir = root.join(glob);
            let entries = fs::read_dir(&dir)
                .map_err(|err| format!("workspace member glob {member:?}: {err}"))?;
            let mut found = false;
            for entry in entries {
                let entry =
                    entry.map_err(|err| format!("workspace member glob {member:?}: {err}"))?;
                if entry.file_type().map_err(|err| err.to_string())?.is_dir()
                    && entry.path().join("Tong.toml").is_file()
                {
                    out.push(entry.path());
                    found = true;
                }
            }
            if !found {
                return Err(format!(
                    "workspace member glob {member:?} matched no directories with Tong.toml"
                ));
            }
        } else {
            let path = root.join(member);
            if !path.join("Tong.toml").is_file() {
                return Err(format!("workspace member {member:?} has no Tong.toml"));
            }
            out.push(path);
        }
    }
    out.sort_by_key(|dir| {
        fs::canonicalize(dir)
            .unwrap_or_else(|_| dir.clone())
            .to_string_lossy()
            .into_owned()
    });
    let mut seen: Vec<PathBuf> = Vec::new();
    let mut deduped: Vec<PathBuf> = Vec::new();
    for dir in out {
        let canonical = fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
        if seen.contains(&canonical) {
            return Err(format!("duplicate workspace member {}", dir.display()));
        }
        seen.push(canonical);
        deduped.push(dir);
    }
    Ok(deduped)
}

/// A package under construction from one or more targets in one member
/// directory (grouped by `package_name` + `version`).
struct PackageSeed {
    dir: PathBuf,
    package_name: String,
    version: String,
    rel: String,
    edition: Option<Edition>,
    lib: Option<LibTarget>,
    bins: Vec<BinTarget>,
    tests: Vec<TestTarget>,
    build_script: Option<PathBuf>,
    deps: Vec<Dep>,
    dev_deps: Vec<Dep>,
    features: BTreeMap<String, Vec<String>>,
    has_default_feature: bool,
    rustflags: Vec<String>,
    env: BTreeMap<String, String>,
}

impl PackageSeed {
    fn add_target(
        &mut self,
        key: &str,
        target: &TargetConfig,
        dir: &Path,
        root: &Path,
        targets_by_label: &BTreeMap<(PathBuf, String), &TargetConfig>,
    ) -> Result<(), String> {
        if !SUPPORTED_RULES.contains(&target.rule.as_str()) {
            return Err(format!(
                "target {key:?} uses unknown rule {:?}; supported rules: {}",
                target.rule,
                SUPPORTED_RULES.join(", ")
            ));
        }
        let edition = parse_edition(target.edition.as_deref());
        // Package-level settings must agree across the grouped targets.
        match &self.edition {
            Some(previous) if *previous != edition => {
                return Err(format!(
                    "package {} declares conflicting editions in its targets ({:?} vs {:?})",
                    self.package_name, previous, edition
                ));
            }
            _ => self.edition = Some(edition),
        }
        if let Some(script) = &target.build_script {
            if self.build_script.is_some() {
                return Err(format!(
                    "package {} declares more than one build script",
                    self.package_name
                ));
            }
            self.build_script = Some(resolve_package_path(dir, target, script, "build_script")?);
        }
        for (name, references) in &target.features {
            if let Some(previous) = self.features.get(name)
                && previous != references
            {
                return Err(format!(
                    "package {} declares conflicting feature {name:?}",
                    self.package_name
                ));
            }
            self.features.insert(name.clone(), references.clone());
        }
        if target.features.contains_key("default") {
            self.has_default_feature = true;
        }
        self.rustflags.extend(target.rustflags.iter().cloned());
        for (name, value) in &target.env {
            self.env.insert(name.clone(), value.clone());
        }
        let deps = resolve_deps(
            &target.deps,
            dir,
            root,
            targets_by_label,
            &self.package_name,
        )?;
        let dev_deps = resolve_deps(
            &target.dev_deps,
            dir,
            root,
            targets_by_label,
            &self.package_name,
        )?;
        self.deps.extend(deps);
        self.dev_deps.extend(dev_deps);

        let crate_root = target
            .crate_root
            .as_ref()
            .map(|root_path| resolve_package_path(dir, target, root_path, "crate_root"))
            .transpose()?;
        match target.rule.as_str() {
            "rust_library" | "rust_proc_macro" => {
                if self.lib.is_some() {
                    return Err(format!(
                        "package {} declares more than one library target",
                        self.package_name
                    ));
                }
                let path = crate_root.unwrap_or_else(|| PathBuf::from("src/lib.rs"));
                let proc_macro = target
                    .proc_macro
                    .unwrap_or(target.rule == "rust_proc_macro");
                let crate_types = parse_crate_types(&target.crate_types);
                self.lib = Some(LibTarget {
                    name: target.crate_name.clone(),
                    crate_types,
                    proc_macro,
                    path,
                });
            }
            "rust_binary" | "rust_example" => {
                let path = crate_root.unwrap_or_else(|| PathBuf::from("src/main.rs"));
                self.bins.push(BinTarget {
                    name: target.output_name.clone().unwrap_or_else(|| key.to_owned()),
                    crate_name: target
                        .crate_name
                        .clone()
                        .unwrap_or_else(|| tong_rust::model::crate_name(&self.package_name)),
                    path,
                    required_features: target.required_features.clone().unwrap_or_default(),
                });
            }
            "rust_test" | "rust_bench" => {
                let path = crate_root.unwrap_or_else(|| {
                    let default_dir = if target.rule == "rust_bench" {
                        "benches"
                    } else {
                        "tests"
                    };
                    PathBuf::from(format!("{default_dir}/{key}.rs"))
                });
                self.tests.push(TestTarget {
                    name: key.to_owned(),
                    path,
                    harness: target.harness.unwrap_or(true),
                    doc: false,
                    cache_test_result: target.cache_test_result.unwrap_or(false),
                    required_features: target.required_features.clone().unwrap_or_default(),
                });
            }
            "rust_doc_test" => {
                let path = crate_root.unwrap_or_else(|| PathBuf::from("src/lib.rs"));
                self.tests.push(TestTarget {
                    name: key.to_owned(),
                    path,
                    harness: false,
                    doc: true,
                    cache_test_result: false,
                    required_features: target.required_features.clone().unwrap_or_default(),
                });
            }
            _ => unreachable!("supported rules checked above"),
        }
        Ok(())
    }

    fn into_package(self) -> Result<Package, String> {
        let version = semver::Version::parse(&self.version).map_err(|err| {
            format!(
                "package {} has invalid version {:?}: {err}",
                self.package_name, self.version
            )
        })?;
        let id = PackageId {
            name: self.package_name.clone(),
            version,
            source: SourceId::Workspace(self.rel.clone()),
        };
        Ok(Package {
            id,
            name: self.package_name,
            dir: self.dir,
            version: self.version,
            edition: self.edition.unwrap_or(Edition::E2021),
            lib: self.lib,
            bins: self.bins,
            tests: self.tests,
            build_script: self.build_script,
            deps: self.deps,
            build_deps: Vec::new(),
            dev_deps: self.dev_deps,
            features: self.features,
            has_default_feature: self.has_default_feature,
            rustflags: self.rustflags,
            env: self.env,
        })
    }
}

/// Resolves a package-relative path (`crate_root`, `build_script`,
/// `cc_import.shared`) beneath the target's `package_root`, rejecting
/// escapes (`..` or symlink canonicalization outside the package root).
///
/// In-package results are returned relative to the member directory (so
/// model paths and compile arguments never carry absolute host paths);
/// external `package_root`s return the absolute path, which the backend
/// mounts under `ext/<n>` deterministically.
fn resolve_package_path(
    member: &Path,
    target: &TargetConfig,
    relative: &str,
    what: &str,
) -> Result<PathBuf, String> {
    let package_root = target
        .package_root
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let base = if package_root.is_absolute() {
        package_root.clone()
    } else {
        member.join(package_root)
    };
    let resolved = base.join(relative);
    let base_canonical = fs::canonicalize(&base).map_err(|err| {
        format!(
            "package_root {:?} does not exist: {err}",
            target.package_root
        )
    })?;
    let resolved_canonical = fs::canonicalize(&resolved)
        .map_err(|err| format!("{what} {relative:?} does not exist: {err}"))?;
    if !resolved_canonical.starts_with(&base_canonical) {
        return Err(format!(
            "{what} {relative:?} escapes the package root {} (.. or symlink outside \
             the package root is rejected)",
            base.display()
        ));
    }
    Ok(resolved
        .strip_prefix(member)
        .unwrap_or(&resolved)
        .to_path_buf())
}

/// Resolves `cc_import` fields; absolute `shared` paths are kept as-is,
/// relative ones resolve beneath `package_root`.
fn resolve_cc_import(key: &str, target: &TargetConfig, dir: &Path) -> Result<CcImport, String> {
    let shared = match &target.shared {
        Some(shared) => {
            let path = PathBuf::from(shared);
            if path.is_absolute() {
                path
            } else {
                // The importer captures the host file directly: the
                // resolved path must be absolute even when it lies inside
                // the member directory.
                let resolved = resolve_package_path(dir, target, shared, "cc_import.shared")?;
                if resolved.is_absolute() {
                    resolved
                } else {
                    dir.join(resolved)
                }
            }
        }
        None => return Err(format!("cc_import target {key:?} requires `shared`")),
    };
    let link_name = target
        .link_name
        .clone()
        .unwrap_or_else(|| default_link_name(&shared));
    Ok(CcImport {
        name: key.to_owned(),
        shared,
        link_name,
    })
}

/// Resolves `deps`/`dev_deps` labels to exact package identities.
fn resolve_deps(
    configs: &[TargetDepConfig],
    dir: &Path,
    root: &Path,
    targets_by_label: &BTreeMap<(PathBuf, String), &TargetConfig>,
    parent: &str,
) -> Result<Vec<Dep>, String> {
    let mut out = Vec::new();
    for config in configs {
        let (label, alias, optional, default_features, features) = match config {
            TargetDepConfig::Label(label) => (label, None, false, true, Vec::new()),
            TargetDepConfig::Table {
                label,
                alias,
                optional,
                default_features,
                features,
            } => (
                label,
                alias.clone(),
                optional.unwrap_or(false),
                default_features.unwrap_or(true),
                features.clone().unwrap_or_default(),
            ),
        };
        let (member, key) = if let Some(rest) = label.strip_prefix("//") {
            let (member_part, name) = match rest.rsplit_once(':') {
                Some((member_part, name)) => (member_part, Some(name.to_owned())),
                None => (rest, None),
            };
            let member: PathBuf = root.join(member_part);
            let key: String = match name {
                Some(name) => name,
                None => {
                    let (key, _) = default_target(&member, root, targets_by_label)?;
                    key.to_owned()
                }
            };
            (member, key)
        } else {
            let name = label.strip_prefix(':').unwrap_or(label);
            (dir.to_path_buf(), name.to_owned())
        };
        let canonical = fs::canonicalize(&member).unwrap_or(member.clone());
        let target = targets_by_label
            .get(&(canonical.clone(), key.clone()))
            .ok_or_else(|| {
                format!(
                    "dependency label {label:?} of package {parent:?} names no target: \
                     no {key:?} in {}",
                    member.display()
                )
            })?;
        let package_name = target.package_name.clone().unwrap_or_else(|| key.clone());
        let version = target.version.clone().unwrap_or_else(|| "0.0.0".to_owned());
        let version = semver::Version::parse(&version)
            .map_err(|err| format!("dependency {package_name:?} has invalid version: {err}"))?;
        let source = SourceId::Workspace(source_rel_path(&canonical, root));
        let package = PackageId {
            name: package_name,
            version,
            source,
        };
        let extern_name = alias.unwrap_or_else(|| key.clone()).replace('-', "_");
        if out.iter().any(|dep: &Dep| dep.extern_name == extern_name) {
            return Err(format!(
                "package {parent:?} declares duplicate extern name {extern_name:?}"
            ));
        }
        out.push(Dep {
            extern_name,
            package,
            optional,
            default_features,
            features,
            target: None,
        });
    }
    Ok(out)
}

/// The default target of a member for a bare `//member/path` label: the
/// `rust_library`, else the target whose package name matches the member
/// directory, else the `rust_binary`.
fn default_target<'a>(
    member: &Path,
    root: &Path,
    targets_by_label: &'a BTreeMap<(PathBuf, String), &TargetConfig>,
) -> Result<(&'a String, &'a TargetConfig), String> {
    let rel = member.strip_prefix(root).unwrap_or(member);
    let base = rel.file_name().and_then(|name| name.to_str()).unwrap_or("");
    let canonical = fs::canonicalize(member).unwrap_or_else(|_| member.to_path_buf());
    let candidates: Vec<(&String, &TargetConfig)> = targets_by_label
        .iter()
        .filter(|((dir, _), _)| *dir == canonical)
        .map(|((_, key), target)| (key, *target))
        .collect();
    if let Some((key, target)) = candidates.iter().find(|(_, t)| t.rule == "rust_library") {
        return Ok((key, target));
    }
    if let Some((key, target)) = candidates
        .iter()
        .find(|(key, target)| target.package_name.as_deref().unwrap_or(key) == base)
    {
        return Ok((key, target));
    }
    if let Some((key, target)) = candidates.iter().find(|(_, t)| t.rule == "rust_binary") {
        return Ok((key, target));
    }
    Err(format!(
        "member {} has no default target for label `//{}` \
         (no rust_library, no target named {base:?})",
        rel.display(),
        rel.display()
    ))
}

fn parse_edition(edition: Option<&str>) -> Edition {
    match edition {
        Some("2015") => Edition::E2015,
        Some("2018") => Edition::E2018,
        Some("2024") => Edition::E2024,
        _ => Edition::E2021,
    }
}

fn parse_crate_types(types: &[String]) -> Vec<tong_rust::model::CrateType> {
    use tong_rust::model::CrateType;
    types
        .iter()
        .filter_map(|kind| match kind.as_str() {
            "rlib" => Some(CrateType::Rlib),
            "cdylib" => Some(CrateType::Cdylib),
            "staticlib" => Some(CrateType::Staticlib),
            "dylib" => Some(CrateType::Dylib),
            _ => None,
        })
        .collect()
}

fn profile_from_config(config: &ProfileConfig) -> ProfileSpec {
    let mut spec = ProfileSpec::dev();
    if let Some(level) = &config.opt_level {
        spec.opt_level = match level {
            OptLevel::Num(n) => n.to_string(),
            OptLevel::Str(s) => s.clone(),
        };
    }
    if let Some(debug) = config.debug {
        spec.debug = debug;
    }
    if let Some(lto) = &config.lto {
        spec.lto = match lto {
            Lto::Bool(true) => tong_rust::model::Lto::Fat,
            Lto::Bool(false) => tong_rust::model::Lto::Off,
            Lto::Str(s) if s == "thin" => tong_rust::model::Lto::Thin,
            Lto::Str(s) if s == "fat" => tong_rust::model::Lto::Fat,
            _ => tong_rust::model::Lto::Off,
        };
    }
    if let Some(panic) = &config.panic {
        spec.panic = match panic.as_str() {
            "abort" => tong_rust::model::PanicStrategy::Abort,
            _ => tong_rust::model::PanicStrategy::Unwind,
        };
    }
    if let Some(units) = config.codegen_units {
        spec.codegen_units = Some(units);
    }
    if let Some(checks) = config.overflow_checks {
        spec.overflow_checks = Some(checks);
    }
    if let Some(assertions) = config.debug_assertions {
        spec.debug_assertions = Some(assertions);
    }
    if let Some(strip) = &config.strip {
        match strip.as_str() {
            "none" | "debuginfo" | "symbols" => spec.strip = Some(strip.clone()),
            other => eprintln!(
                "tong: warning: ignoring invalid strip value {other:?} \
                 (expected \"none\", \"debuginfo\", or \"symbols\")"
            ),
        }
    }
    if let Some(rpath) = config.rpath {
        spec.rpath = Some(rpath);
    }
    spec
}

fn default_link_name(shared: &Path) -> String {
    let stem = shared
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("lib");
    let stem = stem
        .strip_prefix("lib")
        .and_then(|s| s.split_once('.').map(|(name, _)| name))
        .unwrap_or(stem);
    stem.to_owned()
}
