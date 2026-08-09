//! `Cargo.toml` import mode (PLAN.md section 8.1).
//!
//! Cargo manifests are translated into the same [`RustModel`] the native
//! `Tong.toml` targets produce; Cargo is never invoked during a Tong build.
//! Path and workspace dependencies import directly; registry dependencies
//! resolve through `Tong.lock` + the source store (the driver's
//! [`LockedSourceProvider`]). Git dependencies are not yet supported and
//! fail with a targeted diagnostic.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use semver::Version;
use serde::Deserialize;

use crate::model::{
    BinTarget, Dep, Edition, LibTarget, Lto, Package, PanicStrategy, ProfileSpec, RegistryEdge,
    RustModel, TestTarget, lib_crate_name,
};

/// Cargo import failure.
#[derive(Debug)]
pub enum CargoImportError {
    /// The file could not be read.
    Io(String, io::Error),
    /// The file was not valid TOML.
    Parse(String, toml::de::Error),
    /// Unsupported Cargo feature (targeted diagnostic).
    Unsupported(String),
}

impl std::fmt::Display for CargoImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(path, err) => write!(f, "cannot read {path}: {err}"),
            Self::Parse(path, err) => write!(f, "cannot parse {path}: {err}"),
            Self::Unsupported(msg) => write!(f, "unsupported Cargo feature: {msg}"),
        }
    }
}

impl std::error::Error for CargoImportError {}

// --- Cargo manifest shapes (subset) ---------------------------------------

#[derive(Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
struct CargoManifest {
    package: Option<CargoPackage>,
    workspace: Option<CargoWorkspace>,
    #[serde(default)]
    dependencies: BTreeMap<String, DepValue>,
    #[serde(default)]
    build_dependencies: BTreeMap<String, DepValue>,
    #[serde(default)]
    dev_dependencies: BTreeMap<String, DepValue>,
    /// `[target.'cfg(...)'.dependencies]` etc.
    #[serde(default)]
    target: BTreeMap<String, CargoTargetTable>,
    #[serde(default)]
    features: BTreeMap<String, Vec<String>>,
    lib: Option<CargoLib>,
    #[serde(default)]
    bin: Vec<CargoBin>,
    #[serde(default)]
    test: Vec<CargoTest>,
    #[serde(default)]
    bench: Vec<CargoTest>,
    #[serde(default)]
    #[allow(dead_code)]
    example: Vec<CargoExample>,
    #[serde(default)]
    profile: BTreeMap<String, CargoProfile>,
}

/// One `[target.<key>]` table: dependencies scoped to a `cfg(...)`
/// expression or a literal target triple.
#[derive(Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
struct CargoTargetTable {
    #[serde(default)]
    dependencies: BTreeMap<String, DepValue>,
    #[serde(default)]
    build_dependencies: BTreeMap<String, DepValue>,
    #[serde(default)]
    dev_dependencies: BTreeMap<String, DepValue>,
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
struct CargoPackage {
    name: String,
    #[serde(default)]
    version: Option<Field>,
    #[serde(default)]
    edition: Option<Field>,
    build: Option<BuildKey>,
}

/// `build = "build.rs"` or `build = false` (Cargo's opt-out from build.rs
/// auto-detection; `true` is rejected by Cargo too).
#[derive(Deserialize, Clone)]
#[serde(untagged)]
enum BuildKey {
    Path(String),
    Flag(bool),
}

/// A field that is either set inline (`version = "0.1"`) or inherited from
/// `[workspace.package]` (`version.workspace = true`).
#[derive(Deserialize, Clone)]
#[serde(untagged)]
enum Field {
    Value(String),
    Inherit { workspace: bool },
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
struct CargoWorkspace {
    #[serde(default)]
    members: Vec<String>,
    #[serde(default)]
    dependencies: BTreeMap<String, DepValue>,
    /// `[workspace.package]` — defaults inherited by members.
    #[serde(default)]
    package: Option<CargoWorkspacePackage>,
}

/// `[workspace.package]` subset: version and edition.
#[derive(Deserialize, Clone, Default)]
#[serde(rename_all = "kebab-case")]
struct CargoWorkspacePackage {
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    edition: Option<String>,
}

/// Workspace inheritance context: `[workspace.dependencies]` and
/// `[workspace.package]` defaults.
#[derive(Clone, Default)]
struct Inherited {
    deps: BTreeMap<String, DepValue>,
    package: Option<CargoWorkspacePackage>,
}

/// `name = { version = "...", path = "...", workspace = true, package = "..." }`.
#[derive(Deserialize, Clone)]
#[serde(untagged)]
enum DepValue {
    /// `name = "0.1"` — registry dependency.
    Version(String),
    /// A table: path/version/workspace deps (and renames).
    Table {
        version: Option<String>,
        path: Option<String>,
        #[serde(rename = "workspace")]
        workspace: Option<bool>,
        /// Optional `package = "real-name"` rename for path deps.
        package: Option<String>,
        /// Optional dependency (activated via features).
        optional: Option<bool>,
        /// Disable the dependency's default feature.
        #[serde(rename = "default-features")]
        default_features: Option<bool>,
        /// Features requested on the dependency.
        features: Option<Vec<String>>,
        /// Target-specific dependency (`cfg(...)` expression).
        target: Option<String>,
    },
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
struct CargoLib {
    name: Option<String>,
    #[serde(default)]
    crate_type: Vec<String>,
    #[serde(default)]
    proc_macro: bool,
    path: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
struct CargoBin {
    name: Option<String>,
    path: Option<String>,
}

/// `[[test]]` / `[[bench]]` entry. Cargo defaults: path is
/// `tests/<name>.rs` / `benches/<name>.rs`, harness defaults to true.
#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
struct CargoTest {
    name: Option<String>,
    path: Option<String>,
    harness: Option<bool>,
}

/// `[[example]]` entry — parsed and ignored: tong does not build examples
/// (cargo builds them only on demand), but the tables must deserialize.
#[derive(Deserialize, Default)]
#[allow(dead_code)]
#[serde(rename_all = "kebab-case")]
struct CargoExample {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    required_features: Option<Vec<String>>,
    #[serde(default)]
    crate_type: Option<String>,
    #[serde(default)]
    harness: Option<bool>,
    #[serde(default)]
    test: Option<bool>,
    #[serde(default)]
    doc: Option<bool>,
    #[serde(default)]
    edition: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
struct CargoProfile {
    opt_level: Option<OptLevelValue>,
    debug: Option<DebugValue>,
    lto: Option<LtoValue>,
    panic: Option<String>,
    codegen_units: Option<u32>,
    overflow_checks: Option<bool>,
    debug_assertions: Option<bool>,
    strip: Option<String>,
    rpath: Option<bool>,
}

#[derive(Deserialize, Clone)]
#[serde(untagged)]
enum OptLevelValue {
    Num(u8),
    Str(String),
}

#[derive(Deserialize, Clone)]
#[serde(untagged)]
enum DebugValue {
    Bool(bool),
    Num(u8),
}

#[derive(Deserialize, Clone)]
#[serde(untagged)]
enum LtoValue {
    Bool(bool),
    Str(String),
}

// `.cargo/config.toml` subset.
#[derive(Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
struct CargoConfig {
    build: Option<CargoBuild>,
    env: Option<BTreeMap<String, EnvValue>>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
struct CargoBuild {
    rustflags: Option<Vec<String>>,
}

#[derive(Deserialize, Clone)]
#[serde(untagged)]
enum EnvValue {
    Plain(String),
    Table { value: String },
}

/// A source of locked registry packages: version lookup and extracted
/// source directories.
///
/// Implemented by the driver over `Tong.lock` + the source store; `tong
/// lock` uses a collecting provider that records edges instead of
/// resolving them.
pub trait LockedSourceProvider {
    /// The locked version of a registry dep edge. `Ok(None)` means the
    /// edge is not resolved (collecting mode — used by `tong lock`); an
    /// `Err` is a targeted diagnostic (missing lock, missing entry, or a
    /// lockfile out of date).
    fn locked_version(&self, edge: &RegistryEdge) -> Result<Option<Version>, CargoImportError>;
    /// The extracted source directory of a locked package.
    fn source_dir(&self, name: &str, version: &Version) -> Result<PathBuf, CargoImportError>;
}

/// Imports a Cargo workspace into a [`RustModel`].
///
/// `host_triple` is the rustc host triple (`rustc -vV`); it is used to
/// evaluate target-specific dependencies (`[target.'cfg(...)'.dependencies]`
/// and the dep-table `target` key). Cross-compilation is out of scope, so
/// the host triple is the only evaluation context. Registry dependencies
/// resolve through `sources` (`Tong.lock` + the source store).
pub fn import_cargo_workspace(
    workspace_root: &Path,
    host_triple: &str,
    sources: &dyn LockedSourceProvider,
) -> Result<RustModel, CargoImportError> {
    let root_manifest = read_manifest(workspace_root)?;

    // Workspace inheritance: [workspace.dependencies] and [workspace.package].
    let mut inherited = Inherited::default();
    if let Some(workspace) = &root_manifest.workspace {
        inherited.deps.extend(workspace.dependencies.clone());
        inherited.package = workspace.package.clone();
    }

    let members: Vec<PathBuf> = match &root_manifest.workspace {
        Some(workspace) if !workspace.members.is_empty() => {
            expand_members(workspace_root, &workspace.members, &root_manifest.package)?
        }
        Some(_) | None => {
            if root_manifest.package.is_some() {
                vec![workspace_root.to_path_buf()]
            } else {
                return Err(CargoImportError::Unsupported(
                    "Cargo.toml with neither [package] nor [workspace] members".to_owned(),
                ));
            }
        }
    };

    let mut model = RustModel::default();
    // Canonical package dir → imported package. Path dependencies outside
    // the workspace are imported recursively (Cargo semantics), so the
    // graph is closed over every path dep, not just the members.
    let mut packages: BTreeMap<PathBuf, Package> = BTreeMap::new();
    let mut visiting: Vec<PathBuf> = Vec::new();
    for member in &members {
        import_package(
            member,
            true,
            &mut packages,
            &mut visiting,
            &inherited,
            workspace_root,
            host_triple,
            sources,
        )?;
    }

    // The model resolves deps by package name (no version-aware resolution
    // yet): two imported packages with the same name are ambiguous.
    let mut by_name: BTreeMap<&str, &PathBuf> = BTreeMap::new();
    for pkg in packages.values() {
        if let Some(previous) = by_name.insert(pkg.name.as_str(), &pkg.dir) {
            return Err(CargoImportError::Unsupported(format!(
                "two packages named {} ({} and {}); Tong cannot distinguish \
                 same-name packages yet",
                pkg.name,
                previous.display(),
                pkg.dir.display()
            )));
        }
    }

    // Workspace members (feature seeds and lockfile roots).
    let mut member_names: Vec<String> = Vec::new();
    for member in &members {
        let canonical = fs::canonicalize(member)
            .map_err(|err| CargoImportError::Io(member.display().to_string(), err))?;
        if let Some(name) = by_name
            .iter()
            .find(|(_, dir)| ***dir == canonical)
            .map(|(name, _)| *name)
        {
            member_names.push(name.to_owned());
        }
    }
    member_names.sort();

    model.packages = packages.into_values().collect();
    model.members = member_names;

    // Profiles from the workspace root manifest (Cargo: [profile.*] tables).
    model.profiles = resolve_profiles(&root_manifest.profile)?;
    model
        .profiles
        .entry("dev".to_owned())
        .or_insert_with(ProfileSpec::dev);
    model
        .profiles
        .entry("release".to_owned())
        .or_insert_with(ProfileSpec::release);

    // `.cargo/config.toml`: [build] rustflags and [env].
    let config = load_config(workspace_root);
    if let Some(build) = &config.build
        && let Some(flags) = &build.rustflags
    {
        model.global_rustflags = flags.clone();
    }
    if let Some(env) = &config.env {
        for (key, value) in env {
            let value = match value {
                EnvValue::Plain(v) | EnvValue::Table { value: v } => v.clone(),
            };
            model.global_env.insert(key.clone(), value);
        }
    }

    Ok(model)
}

fn read_manifest(dir: &Path) -> Result<CargoManifest, CargoImportError> {
    let path = dir.join("Cargo.toml");
    let text = fs::read_to_string(&path)
        .map_err(|err| CargoImportError::Io(path.display().to_string(), err))?;
    toml::from_str(&text).map_err(|err| CargoImportError::Parse(path.display().to_string(), err))
}

fn expand_members(
    root: &Path,
    members: &[String],
    root_package: &Option<CargoPackage>,
) -> Result<Vec<PathBuf>, CargoImportError> {
    let mut out = Vec::new();
    // The root itself may also be a member (workspace root package).
    if root_package.is_some() {
        out.push(root.to_path_buf());
    }
    for member in members {
        if let Some(glob) = member.strip_suffix("/*") {
            let dir = root.join(glob);
            let entries = fs::read_dir(&dir)
                .map_err(|err| CargoImportError::Io(dir.display().to_string(), err))?;
            let mut found = false;
            for entry in entries {
                let entry = entry.map_err(|err| CargoImportError::Io(String::new(), err))?;
                if entry
                    .file_type()
                    .map_err(|err| CargoImportError::Io(String::new(), err))?
                    .is_dir()
                    && entry.path().join("Cargo.toml").is_file()
                {
                    out.push(entry.path());
                    found = true;
                }
            }
            if !found {
                return Err(CargoImportError::Unsupported(format!(
                    "workspace member glob {member:?} matched nothing"
                )));
            }
        } else {
            let path = root.join(member);
            if path.join("Cargo.toml").is_file() {
                out.push(path);
            } else {
                return Err(CargoImportError::Unsupported(format!(
                    "workspace member {member:?} has no Cargo.toml"
                )));
            }
        }
    }
    Ok(out)
}

/// Imports the package at `dir` (a workspace member or a path dependency)
/// into `packages`, recursing into its path dependencies. Returns the
/// package's declared name. Cycles are rejected, matching Cargo.
#[allow(clippy::too_many_arguments)]
fn import_package(
    dir: &Path,
    is_member: bool,
    packages: &mut BTreeMap<PathBuf, Package>,
    visiting: &mut Vec<PathBuf>,
    inherited: &Inherited,
    workspace_root: &Path,
    host_triple: &str,
    sources: &dyn LockedSourceProvider,
) -> Result<String, CargoImportError> {
    let canonical = fs::canonicalize(dir)
        .map_err(|err| CargoImportError::Io(dir.display().to_string(), err))?;
    if let Some(pkg) = packages.get(&canonical) {
        return Ok(pkg.name.clone());
    }
    if visiting.contains(&canonical) {
        return Err(CargoImportError::Unsupported(format!(
            "cyclic path dependency involving {}",
            dir.display()
        )));
    }
    visiting.push(canonical.clone());

    let result = (|| {
        let manifest = read_manifest(&canonical)?;
        // A path dependency may be its own workspace root; its
        // `[workspace.dependencies]`/`[workspace.package]` then apply,
        // not the importer's.
        let inherited = match &manifest.workspace {
            Some(workspace) => Inherited {
                deps: workspace.dependencies.clone(),
                package: workspace.package.clone(),
            },
            None => inherited.clone(),
        };
        let package = manifest.package.as_ref().ok_or_else(|| {
            CargoImportError::Unsupported(format!(
                "{} declares a workspace but not a package",
                canonical.display()
            ))
        })?;

        let version = resolve_field(
            &package.version,
            inherited.package.as_ref(),
            &package.name,
            "version",
            &canonical,
            "0.0.0",
        )?;
        let edition = resolve_field(
            &package.edition,
            inherited.package.as_ref(),
            &package.name,
            "edition",
            &canonical,
            "2015",
        )?;

        let mut pkg = Package {
            name: package.name.clone(),
            dir: canonical.clone(),
            version,
            edition: parse_edition(&edition)?,
            lib: None,
            bins: Vec::new(),
            tests: Vec::new(),
            build_script: None,
            deps: Vec::new(),
            build_deps: Vec::new(),
            dev_deps: Vec::new(),
            features: manifest.features.clone(),
            has_default_feature: manifest.features.contains_key("default"),
            rustflags: Vec::new(),
            env: BTreeMap::new(),
        };
        // Build script: explicit path, `build = false` opt-out, or Cargo's
        // auto-detection of `build.rs` at the package root.
        pkg.build_script = match &package.build {
            Some(BuildKey::Path(path)) => Some(PathBuf::from(path)),
            Some(BuildKey::Flag(false)) => None,
            Some(BuildKey::Flag(true)) => {
                return Err(CargoImportError::Unsupported(format!(
                    "package {} in {} sets build = true; Cargo requires a \
                     path or false",
                    package.name,
                    canonical.display()
                )));
            }
            None => (pkg.dir.join("build.rs").is_file()).then(|| PathBuf::from("build.rs")),
        };

        // Library target: explicit [lib] or auto-detected src/lib.rs.
        let lib_path = match &manifest.lib {
            Some(lib) => lib
                .path
                .clone()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("src/lib.rs")),
            None => PathBuf::from("src/lib.rs"),
        };
        let lib_present = pkg.dir.join(&lib_path).is_file();
        if let Some(lib) = &manifest.lib {
            pkg.lib = Some(LibTarget {
                name: lib.name.clone(),
                crate_types: parse_crate_types(&lib.crate_type)?,
                proc_macro: lib.proc_macro,
                path: lib_path,
            });
        } else if lib_present {
            pkg.lib = Some(LibTarget {
                name: None,
                crate_types: Vec::new(),
                proc_macro: false,
                path: lib_path,
            });
        }

        // Binaries: explicit [[bin]] or auto-detected src/main.rs.
        if manifest.bin.is_empty() && pkg.dir.join("src/main.rs").is_file() {
            pkg.bins.push(BinTarget {
                name: package.name.clone(),
                path: PathBuf::from("src/main.rs"),
            });
        }
        for bin in &manifest.bin {
            let name = bin.name.clone().unwrap_or_else(|| {
                bin.path
                    .as_ref()
                    .and_then(|p| Path::new(p).file_stem())
                    .and_then(|s| s.to_str())
                    .unwrap_or(&package.name)
                    .to_owned()
            });
            let path = bin
                .path
                .clone()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(format!("src/bin/{name}.rs")));
            pkg.bins.push(BinTarget { name, path });
        }

        // Test targets: [[test]] / [[bench]] entries whose source exists
        // (Cargo drops targets without source files), plus the auto-derived
        // lib unit test. [[example]] targets are ignored (tong does not
        // build examples; cargo builds them only on demand).
        for (entry, default_dir, kind) in [
            (&manifest.test, "tests", "test"),
            (&manifest.bench, "benches", "bench"),
        ] {
            for target in entry {
                let name = target.name.clone().unwrap_or_else(|| {
                    target
                        .path
                        .as_ref()
                        .and_then(|p| Path::new(p).file_stem())
                        .and_then(|s| s.to_str())
                        .unwrap_or(&package.name)
                        .to_owned()
                });
                let path = target
                    .path
                    .clone()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from(format!("{default_dir}/{name}.rs")));
                if !pkg.dir.join(&path).is_file() {
                    // Cargo silently drops test targets whose source is
                    // missing.
                    continue;
                }
                pkg.tests.push(TestTarget {
                    name,
                    path,
                    harness: target.harness.unwrap_or(true),
                });
                let _ = kind;
            }
        }
        // The package's own library unit test (Cargo: `--test` on the lib).
        if let Some(lib) = &pkg.lib {
            pkg.tests.push(TestTarget {
                name: lib_crate_name(&pkg),
                path: lib.path.clone(),
                harness: true,
            });
        }

        // Dependencies (path and workspace-inherited only), with
        // target-specific tables merged in for matching targets. Direct
        // paths resolve relative to this manifest; inherited paths resolve
        // relative to the workspace root.
        let (dependencies, build_dependencies, dev_dependencies) =
            merge_target_tables(&manifest, host_triple)?;
        let resolved_deps = resolve_deps(
            &dependencies,
            &pkg.name,
            &canonical,
            workspace_root,
            &inherited.deps,
            host_triple,
            sources,
        )?;
        let resolved_build_deps = resolve_deps(
            &build_dependencies,
            &pkg.name,
            &canonical,
            workspace_root,
            &inherited.deps,
            host_triple,
            sources,
        )?;
        let resolved_dev_deps = if is_member {
            resolve_deps(
                &dev_dependencies,
                &pkg.name,
                &canonical,
                workspace_root,
                &inherited.deps,
                host_triple,
                sources,
            )?
        } else {
            Vec::new()
        };
        let mut deps = Vec::new();
        let mut build_deps = Vec::new();
        let mut dev_deps = Vec::new();
        for (resolved, target) in [
            (resolved_deps, &mut deps),
            (resolved_build_deps, &mut build_deps),
            (resolved_dev_deps, &mut dev_deps),
        ] {
            for dep in resolved {
                let real_name = match dep.path {
                    Some(path) => {
                        let imported = import_package(
                            &path,
                            false,
                            packages,
                            visiting,
                            &inherited,
                            workspace_root,
                            host_triple,
                            sources,
                        )?;
                        if imported != dep.package {
                            return Err(CargoImportError::Unsupported(format!(
                                "path dependency {} = {{ path = {:?} }} resolves to package \
                                 {imported:?}, not {:?}",
                                dep.extern_name.replace('_', "-"),
                                path.display(),
                                dep.package
                            )));
                        }
                        imported
                    }
                    None => {
                        // Registry dependency: resolved through the
                        // lockfile-backed source provider. The provider
                        // already validated the version requirement.
                        let Some(version) = dep.locked_version else {
                            // Collecting mode (`tong lock`): the edge was
                            // recorded; no package is imported.
                            continue;
                        };
                        let dir = sources.source_dir(&dep.package, &version)?;
                        let imported = import_package(
                            &dir,
                            false,
                            packages,
                            visiting,
                            &inherited,
                            workspace_root,
                            host_triple,
                            sources,
                        )?;
                        if imported != dep.package {
                            return Err(CargoImportError::Unsupported(format!(
                                "locked dependency {} = {{ version = {:?} }} resolves to \
                                 package {imported:?}, not {:?}",
                                dep.extern_name.replace('_', "-"),
                                version,
                                dep.package
                            )));
                        }
                        imported
                    }
                };
                target.push(Dep {
                    extern_name: dep.extern_name,
                    package: real_name,
                    optional: dep.optional,
                    default_features: dep.default_features,
                    features: dep.features,
                    target: dep.target,
                });
            }
        }
        pkg.deps = deps;
        pkg.build_deps = build_deps;
        pkg.dev_deps = dev_deps;

        let name = pkg.name.clone();
        packages.insert(canonical, pkg);
        Ok(name)
    })();

    visiting.pop();
    result
}

/// Resolves a package field that may be inherited from
/// `[workspace.package]`, with Cargo-compatible diagnostics.
fn resolve_field(
    field: &Option<Field>,
    workspace_package: Option<&CargoWorkspacePackage>,
    package_name: &str,
    what: &str,
    dir: &Path,
    default: &str,
) -> Result<String, CargoImportError> {
    match field {
        None => Ok(default.to_owned()),
        Some(Field::Value(value)) => Ok(value.clone()),
        Some(Field::Inherit { workspace: true }) => {
            let inherited = workspace_package
                .and_then(|package| match what {
                    "version" => package.version.clone(),
                    "edition" => package.edition.clone(),
                    _ => None,
                })
                .ok_or_else(|| {
                    CargoImportError::Unsupported(format!(
                        "package {package_name:?} in {} inherits {what} from \
                         [workspace.package], which defines none",
                        dir.display()
                    ))
                })?;
            Ok(inherited)
        }
        Some(Field::Inherit { workspace: false }) => Err(CargoImportError::Unsupported(format!(
            "package {package_name:?} in {} sets {what}.workspace = false",
            dir.display()
        ))),
    }
}

/// A resolved dependency: the crate name used at the use site, the package
/// name it refers to, and — for path dependencies — the package directory;
/// registry dependencies carry their locked version.
struct ResolvedDep {
    extern_name: String,
    package: String,
    path: Option<PathBuf>,
    optional: bool,
    default_features: bool,
    features: Vec<String>,
    target: Option<String>,
    locked_version: Option<semver::Version>,
}

fn resolve_deps(
    deps: &BTreeMap<String, DepValue>,
    parent_name: &str,
    member: &Path,
    workspace_root: &Path,
    inherited: &BTreeMap<String, DepValue>,
    _host_triple: &str,
    sources: &dyn LockedSourceProvider,
) -> Result<Vec<ResolvedDep>, CargoImportError> {
    let mut out = Vec::new();
    for (name, value) in deps {
        // The effective table: the member's own table merged over the
        // inherited `[workspace.dependencies]` table (Cargo semantics:
        // features concatenate, other keys override).
        let (path, package, optional, default_features, features, target, req) = match value {
            DepValue::Version(version) => {
                // Registry dependency: resolved through the lockfile.
                let edge = RegistryEdge {
                    parent: parent_name.to_owned(),
                    extern_name: name.replace('-', "_"),
                    package: name.clone(),
                    req: version.clone(),
                    optional: false,
                    default_features: true,
                    features: Vec::new(),
                };
                let locked = sources.locked_version(&edge)?;
                (
                    None,
                    name.clone(),
                    false,
                    true,
                    Vec::new(),
                    None,
                    Some((edge, locked)),
                )
            }
            DepValue::Table {
                path,
                version,
                workspace,
                package,
                optional,
                default_features,
                features,
                target,
            } => {
                // Target-specific deps stay in the model with their target
                // recorded (the backend filters them at plan time); the
                // feature walk and the lock need them regardless of host
                // (cargo locks all-target deps).
                let optional = optional.unwrap_or(false);
                let default_features = default_features.unwrap_or(true);
                let features = features.clone().unwrap_or_default();
                if let Some(path) = path {
                    // Path deps resolve relative to the declaring manifest.
                    (
                        Some(member.join(path)),
                        package.clone().unwrap_or_else(|| name.clone()),
                        optional,
                        default_features,
                        features,
                        target.clone(),
                        None,
                    )
                } else if *workspace == Some(true) {
                    match inherited.get(name) {
                        Some(DepValue::Table {
                            path: Some(path),
                            package: inherited_package,
                            optional: inherited_optional,
                            default_features: inherited_default_features,
                            features: inherited_features,
                            ..
                        }) => {
                            // Inherited path deps resolve relative to the
                            // workspace root manifest.
                            let mut merged_features =
                                inherited_features.clone().unwrap_or_default();
                            for feature in &features {
                                if !merged_features.contains(feature) {
                                    merged_features.push(feature.clone());
                                }
                            }
                            (
                                Some(workspace_root.join(path)),
                                inherited_package.clone().unwrap_or_else(|| name.clone()),
                                optional || inherited_optional.unwrap_or(false),
                                default_features && inherited_default_features.unwrap_or(true),
                                merged_features,
                                target.clone(),
                                None,
                            )
                        }
                        Some(DepValue::Version(version)) => {
                            // Inherited registry dependency.
                            let edge = RegistryEdge {
                                parent: parent_name.to_owned(),
                                extern_name: name.replace('-', "_"),
                                package: name.clone(),
                                req: version.clone(),
                                optional,
                                default_features,
                                features: features.clone(),
                            };
                            let locked = sources.locked_version(&edge)?;
                            (
                                None,
                                name.clone(),
                                optional,
                                default_features,
                                features,
                                target.clone(),
                                Some((edge, locked)),
                            )
                        }
                        Some(DepValue::Table {
                            version: Some(version),
                            ..
                        }) => {
                            let edge = RegistryEdge {
                                parent: parent_name.to_owned(),
                                extern_name: name.replace('-', "_"),
                                package: package.clone().unwrap_or_else(|| name.clone()),
                                req: version.clone(),
                                optional,
                                default_features,
                                features: features.clone(),
                            };
                            let locked = sources.locked_version(&edge)?;
                            (
                                None,
                                package.clone().unwrap_or_else(|| name.clone()),
                                optional,
                                default_features,
                                features,
                                target.clone(),
                                Some((edge, locked)),
                            )
                        }
                        _ => {
                            return Err(CargoImportError::Unsupported(format!(
                                "dependency {name:?} uses workspace inheritance without a \
                                 path dependency in [workspace.dependencies]"
                            )));
                        }
                    }
                } else if let Some(version) = version {
                    // Registry dependency in a table form.
                    let edge = RegistryEdge {
                        parent: parent_name.to_owned(),
                        extern_name: name.replace('-', "_"),
                        package: package.clone().unwrap_or_else(|| name.clone()),
                        req: version.clone(),
                        optional,
                        default_features,
                        features: features.clone(),
                    };
                    let locked = sources.locked_version(&edge)?;
                    (
                        None,
                        package.clone().unwrap_or_else(|| name.clone()),
                        optional,
                        default_features,
                        features,
                        target.clone(),
                        Some((edge, locked)),
                    )
                } else {
                    return Err(CargoImportError::Unsupported(format!(
                        "dependency {name:?} in {} is neither a path nor workspace \
                         dependency",
                        member.display()
                    )));
                }
            }
        };
        if let Some(path) = path {
            out.push(ResolvedDep {
                extern_name: name.replace('-', "_"),
                package,
                path: Some(path),
                optional,
                default_features,
                features,
                target,
                locked_version: None,
            });
        } else if let Some((_edge, locked)) = req
            && locked.is_some()
        {
            out.push(ResolvedDep {
                extern_name: name.replace('-', "_"),
                package,
                path: None,
                optional,
                default_features,
                features,
                target,
                locked_version: locked,
            });
        }
        // Collecting mode (`tong lock`): `locked` is None — the edge
        // was recorded by the provider; nothing is imported.
    }
    Ok(out)
}

/// The manifest's dependency tables after target-specific merging.
type MergedTables = (
    BTreeMap<String, DepValue>,
    BTreeMap<String, DepValue>,
    BTreeMap<String, DepValue>,
);

/// Merges the manifest's dependency tables with its matching
/// `[target.'cfg(...)'.dependencies]` tables (target-specific entries
/// override same-name general entries, like Cargo).
fn merge_target_tables(
    manifest: &CargoManifest,
    host_triple: &str,
) -> Result<MergedTables, CargoImportError> {
    let mut dependencies = manifest.dependencies.clone();
    let mut build_dependencies = manifest.build_dependencies.clone();
    let mut dev_dependencies = manifest.dev_dependencies.clone();
    for (key, table) in &manifest.target {
        // Every target table's deps enter the model with the target key
        // recorded (the backend filters at plan time), so the feature walk
        // and the lock see all targets. Cargo's override semantics: a
        // target-specific dep replaces the same-name general dep only when
        // the target matches the host; on a non-matching host the general
        // dep applies (tokio's `tokio_unstable`-gated mio must not shadow
        // the real one).
        let matching = target_matches(key, host_triple, &format!("target table {key:?}"))?;
        let with_target = |dep: &DepValue| -> DepValue {
            let mut dep = dep.clone();
            if let DepValue::Table { target, .. } = &mut dep {
                *target = Some(key.clone());
            }
            dep
        };
        for (name, dep) in &table.dependencies {
            let dep = with_target(dep);
            if matching {
                dependencies.insert(name.clone(), dep);
            } else {
                dependencies.entry(name.clone()).or_insert(dep);
            }
        }
        for (name, dep) in &table.build_dependencies {
            let dep = with_target(dep);
            if matching {
                build_dependencies.insert(name.clone(), dep);
            } else {
                build_dependencies.entry(name.clone()).or_insert(dep);
            }
        }
        for (name, dep) in &table.dev_dependencies {
            let dep = with_target(dep);
            if matching {
                dev_dependencies.insert(name.clone(), dep);
            } else {
                dev_dependencies.entry(name.clone()).or_insert(dep);
            }
        }
    }
    Ok((dependencies, build_dependencies, dev_dependencies))
}

/// Whether a target key (`cfg(...)` expression or literal triple) matches
/// the host triple.
pub(crate) fn target_matches(
    key: &str,
    host_triple: &str,
    what: &str,
) -> Result<bool, CargoImportError> {
    if key.trim_start().starts_with("cfg(") {
        tong_core::platform::eval_cfg(key, host_triple)
            .map_err(|err| CargoImportError::Unsupported(format!("{what}: {}", err)))
    } else if key.contains('-') {
        // A literal target triple: matches only the host (cross-compilation
        // is out of scope).
        Ok(key == host_triple)
    } else {
        Err(CargoImportError::Unsupported(format!(
            "{what}: unsupported target key {key:?}"
        )))
    }
}

fn parse_crate_types(types: &[String]) -> Result<Vec<crate::model::CrateType>, CargoImportError> {
    let mut out = Vec::new();
    for kind in types {
        let crate_type = match kind.as_str() {
            "rlib" => crate::model::CrateType::Rlib,
            "cdylib" => crate::model::CrateType::Cdylib,
            "staticlib" => crate::model::CrateType::Staticlib,
            "dylib" => crate::model::CrateType::Dylib,
            "lib" | "bin" => continue,
            other => {
                return Err(CargoImportError::Unsupported(format!(
                    "crate-type {other:?}"
                )));
            }
        };
        out.push(crate_type);
    }
    Ok(out)
}

fn parse_edition(text: &str) -> Result<Edition, CargoImportError> {
    match text {
        "2015" => Ok(Edition::E2015),
        "2018" => Ok(Edition::E2018),
        "2021" => Ok(Edition::E2021),
        "2024" => Ok(Edition::E2024),
        other => Err(CargoImportError::Unsupported(format!("edition {other:?}"))),
    }
}

fn resolve_profiles(
    tables: &BTreeMap<String, CargoProfile>,
) -> Result<BTreeMap<String, ProfileSpec>, CargoImportError> {
    let mut out = BTreeMap::new();
    for (name, table) in tables {
        if name == "package" {
            continue;
        }
        let mut spec = if name == "release" {
            ProfileSpec::release()
        } else {
            ProfileSpec::dev()
        };
        if let Some(level) = &table.opt_level {
            spec.opt_level = match level {
                OptLevelValue::Num(n) => n.to_string(),
                OptLevelValue::Str(s) => s.clone(),
            };
        }
        if let Some(debug) = &table.debug {
            spec.debug = match debug {
                DebugValue::Bool(b) => *b,
                DebugValue::Num(n) => *n > 0,
            };
        }
        if let Some(lto) = &table.lto {
            spec.lto = match lto {
                LtoValue::Bool(true) => Lto::Fat,
                LtoValue::Bool(false) => Lto::Off,
                LtoValue::Str(s) if s == "thin" => Lto::Thin,
                LtoValue::Str(s) if s == "fat" => Lto::Fat,
                LtoValue::Str(s) if s == "off" => Lto::Off,
                LtoValue::Str(s) if s == "false" => Lto::Off,
                LtoValue::Str(s) if s == "true" => Lto::Fat,
                LtoValue::Str(s) => {
                    eprintln!("tong: ignoring unsupported lto value {s:?}");
                    Lto::Off
                }
            };
        }
        if let Some(panic) = &table.panic {
            spec.panic = match panic.as_str() {
                "unwind" => PanicStrategy::Unwind,
                "abort" => PanicStrategy::Abort,
                other => {
                    eprintln!("tong: ignoring unsupported panic strategy {other:?}");
                    PanicStrategy::Unwind
                }
            };
        }
        if let Some(units) = table.codegen_units {
            spec.codegen_units = Some(units);
        }
        if let Some(checks) = table.overflow_checks {
            spec.overflow_checks = Some(checks);
        }
        if let Some(assertions) = table.debug_assertions {
            spec.debug_assertions = Some(assertions);
        }
        if let Some(strip) = &table.strip {
            match strip.as_str() {
                "none" | "debuginfo" | "symbols" => spec.strip = Some(strip.clone()),
                other => {
                    return Err(CargoImportError::Unsupported(format!(
                        "profile {name:?} strip = {other:?}; expected \"none\", \
                         \"debuginfo\", or \"symbols\""
                    )));
                }
            }
        }
        if let Some(rpath) = table.rpath {
            spec.rpath = Some(rpath);
        }
        out.insert(name.clone(), spec);
    }
    Ok(out)
}

fn load_config(workspace_root: &Path) -> CargoConfig {
    let path = workspace_root.join(".cargo").join("config.toml");
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(_) => return CargoConfig::default(),
    };
    toml::from_str(&text).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Provider for provider-free tests: registry deps are rejected with
    /// the targeted "requires Tong.lock" diagnostic.
    struct NoLock;

    impl LockedSourceProvider for NoLock {
        fn locked_version(&self, edge: &RegistryEdge) -> Result<Option<Version>, CargoImportError> {
            Err(CargoImportError::Unsupported(format!(
                "registry dependency `{}` requires Tong.lock; run `tong lock`",
                edge.package
            )))
        }

        fn source_dir(&self, _name: &str, _version: &Version) -> Result<PathBuf, CargoImportError> {
            Err(CargoImportError::Unsupported(
                "no locked source provider".to_owned(),
            ))
        }
    }

    const NO_LOCK: NoLock = NoLock;

    fn write_tree(files: &[(&str, &str)]) -> tempfile::TempDir {
        // The returned tempdir owns the fixture's parent: entries may
        // reference "../" siblings (path dependencies), and those siblings
        // must never collide with other fixtures in the shared system temp
        // root (concurrent test binaries and stale dirs from earlier runs
        // used to corrupt them).
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("ws");
        fs::create_dir_all(&root).unwrap();
        for (path, content) in files {
            let full = root.join(path);
            fs::create_dir_all(full.parent().unwrap()).unwrap();
            fs::write(full, content).unwrap();
        }
        parent
    }

    #[test]
    fn imports_a_path_dependency_workspace() {
        let dir = write_tree(&[
            (
                "Cargo.toml",
                r#"
[workspace]
members = ["crates/calc-core", "crates/calc-cli"]
"#,
            ),
            (
                "crates/calc-core/Cargo.toml",
                r#"
[package]
name = "calc-core"
version = "0.1.0"
edition = "2021"
"#,
            ),
            (
                "crates/calc-core/src/lib.rs",
                "pub fn add(a: i32, b: i32) -> i32 { a + b }",
            ),
            (
                "crates/calc-cli/Cargo.toml",
                r#"
[package]
name = "calc-cli"
version = "0.1.0"
edition = "2021"

[dependencies]
calc-core = { path = "../calc-core" }
"#,
            ),
            ("crates/calc-cli/src/main.rs", "fn main() {}"),
        ]);
        let model =
            import_cargo_workspace(&dir.path().join("ws"), "aarch64-apple-darwin", &NO_LOCK)
                .unwrap();
        assert_eq!(model.packages.len(), 2);
        let cli = model
            .packages
            .iter()
            .find(|p| p.name == "calc-cli")
            .unwrap();
        assert_eq!(cli.deps.len(), 1);
        assert_eq!(cli.deps[0].extern_name, "calc_core");
        assert_eq!(cli.deps[0].package, "calc-core");
        let core = model
            .packages
            .iter()
            .find(|p| p.name == "calc-core")
            .unwrap();
        assert!(core.lib.is_some());
        assert_eq!(cli.bins.len(), 1);
        // dev profile defaults exist.
        assert!(model.profiles.contains_key("dev"));
        assert!(model.profiles.contains_key("release"));
    }

    #[test]
    fn registry_deps_fail_with_a_targeted_diagnostic() {
        let dir = write_tree(&[
            (
                "Cargo.toml",
                r#"
[package]
name = "app"
version = "0.1.0"
edition = "2021"

[dependencies]
serde = "1"
"#,
            ),
            ("src/main.rs", "fn main() {}"),
        ]);
        let err = import_cargo_workspace(&dir.path().join("ws"), "aarch64-apple-darwin", &NO_LOCK)
            .unwrap_err();
        assert!(err.to_string().contains("registry dependency"), "{err}");
    }

    #[test]
    fn reads_config_rustflags_and_env() {
        let dir = write_tree(&[
            (
                "Cargo.toml",
                r#"
[package]
name = "app"
version = "0.1.0"
edition = "2021"
"#,
            ),
            ("src/main.rs", "fn main() {}"),
            (
                ".cargo/config.toml",
                r#"
[build]
rustflags = ["--cfg", "advanced_mode"]

[env]
APP_GREETING = "hello"
"#,
            ),
        ]);
        let model =
            import_cargo_workspace(&dir.path().join("ws"), "aarch64-apple-darwin", &NO_LOCK)
                .unwrap();
        assert_eq!(model.global_rustflags, vec!["--cfg", "advanced_mode"]);
        assert_eq!(model.global_env.get("APP_GREETING").unwrap(), "hello");
    }

    #[test]
    fn imports_external_path_dependency() {
        // A path dep outside the workspace is imported recursively, like
        // Cargo does (used by examples/04-voxel-city → sdl3-sys).
        let dir = write_tree(&[
            ("Cargo.toml", "[workspace]\nmembers = [\"crates/app\"]\n"),
            (
                "crates/app/Cargo.toml",
                r#"
[package]
name = "app"
version = "0.1.0"
edition = "2021"

[dependencies]
sdl3-sys = { path = "../../shared/sdl3-sys" }
"#,
            ),
            ("crates/app/src/main.rs", "fn main() {}"),
            (
                "shared/sdl3-sys/Cargo.toml",
                "[package]\nname = \"sdl3-sys\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            ),
            ("shared/sdl3-sys/src/lib.rs", "pub fn init() {}"),
        ]);
        let model =
            import_cargo_workspace(&dir.path().join("ws"), "aarch64-apple-darwin", &NO_LOCK)
                .unwrap();
        assert_eq!(model.packages.len(), 2);
        let app = model.packages.iter().find(|p| p.name == "app").unwrap();
        assert_eq!(app.deps.len(), 1);
        assert_eq!(app.deps[0].extern_name, "sdl3_sys");
        assert_eq!(app.deps[0].package, "sdl3-sys");
        let sys = model
            .packages
            .iter()
            .find(|p| p.name == "sdl3-sys")
            .unwrap();
        assert_eq!(
            sys.dir,
            fs::canonicalize(dir.path().join("ws/shared/sdl3-sys")).unwrap()
        );
        assert!(sys.lib.is_some());
    }

    #[test]
    fn rejects_cyclic_path_dependencies() {
        let dir = write_tree(&[
            ("Cargo.toml", "[workspace]\nmembers = [\"a\"]\n"),
            (
                "a/Cargo.toml",
                r#"
[package]
name = "a"
version = "0.1.0"
edition = "2021"

[dependencies]
b = { path = "../b" }
"#,
            ),
            ("a/src/lib.rs", ""),
            (
                "b/Cargo.toml",
                r#"
[package]
name = "b"
version = "0.1.0"
edition = "2021"

[dependencies]
a = { path = "../a" }
"#,
            ),
            ("b/src/lib.rs", ""),
        ]);
        let err = import_cargo_workspace(&dir.path().join("ws"), "aarch64-apple-darwin", &NO_LOCK)
            .unwrap_err();
        assert!(err.to_string().contains("cyclic"), "{err}");
    }

    #[test]
    fn imports_workspace_inherited_version_and_edition() {
        let dir = write_tree(&[
            (
                "Cargo.toml",
                r#"
[workspace]
members = ["app"]

[workspace.package]
version = "1.2.3"
edition = "2021"
"#,
            ),
            (
                "app/Cargo.toml",
                r#"
[package]
name = "app"
version.workspace = true
edition.workspace = true
"#,
            ),
            ("app/src/lib.rs", ""),
        ]);
        let model =
            import_cargo_workspace(&dir.path().join("ws"), "aarch64-apple-darwin", &NO_LOCK)
                .unwrap();
        let app = model.packages.iter().find(|p| p.name == "app").unwrap();
        assert_eq!(app.version, "1.2.3");
        assert_eq!(app.edition, Edition::E2021);
    }

    #[test]
    fn missing_workspace_package_inheritance_is_a_targeted_error() {
        let dir = write_tree(&[
            ("Cargo.toml", "[workspace]\nmembers = [\"app\"]\n"),
            (
                "app/Cargo.toml",
                r#"
[package]
name = "app"
version.workspace = true
"#,
            ),
            ("app/src/lib.rs", ""),
        ]);
        let err = import_cargo_workspace(&dir.path().join("ws"), "aarch64-apple-darwin", &NO_LOCK)
            .unwrap_err();
        assert!(err.to_string().contains("inherits version"), "{err}");
    }

    #[test]
    fn imports_lib_name_override() {
        let dir = write_tree(&[
            (
                "Cargo.toml",
                r#"
[workspace]
members = ["app"]
"#,
            ),
            (
                "app/Cargo.toml",
                r#"
[package]
name = "app"
version = "0.1.0"
edition = "2021"

[lib]
name = "app_core"
"#,
            ),
            ("app/src/lib.rs", ""),
        ]);
        let model =
            import_cargo_workspace(&dir.path().join("ws"), "aarch64-apple-darwin", &NO_LOCK)
                .unwrap();
        let app = model.packages.iter().find(|p| p.name == "app").unwrap();
        assert_eq!(app.lib.as_ref().unwrap().name.as_deref(), Some("app_core"));
    }

    #[test]
    fn auto_detects_build_rs_without_a_build_key() {
        let dir = write_tree(&[
            (
                "Cargo.toml",
                r#"
[workspace]
members = ["app"]
"#,
            ),
            (
                "app/Cargo.toml",
                "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            ),
            ("app/src/lib.rs", ""),
            ("app/build.rs", "fn main() {}"),
        ]);
        let model =
            import_cargo_workspace(&dir.path().join("ws"), "aarch64-apple-darwin", &NO_LOCK)
                .unwrap();
        let app = model.packages.iter().find(|p| p.name == "app").unwrap();
        assert_eq!(
            app.build_script.as_deref(),
            Some(std::path::Path::new("build.rs"))
        );
    }

    #[test]
    fn build_false_disables_auto_detection() {
        let dir = write_tree(&[
            (
                "Cargo.toml",
                r#"
[workspace]
members = ["app"]
"#,
            ),
            (
                "app/Cargo.toml",
                "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\nbuild = false\n",
            ),
            ("app/src/lib.rs", ""),
            // A build.rs exists, but the manifest opts out (Cargo semantics).
            ("app/build.rs", "fn main() {}"),
        ]);
        let model =
            import_cargo_workspace(&dir.path().join("ws"), "aarch64-apple-darwin", &NO_LOCK)
                .unwrap();
        let app = model.packages.iter().find(|p| p.name == "app").unwrap();
        assert!(app.build_script.is_none());
    }

    #[test]
    fn build_true_is_a_targeted_error() {
        let dir = write_tree(&[
            (
                "Cargo.toml",
                r#"
[workspace]
members = ["app"]
"#,
            ),
            (
                "app/Cargo.toml",
                "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\nbuild = true\n",
            ),
            ("app/src/lib.rs", ""),
        ]);
        let err = import_cargo_workspace(&dir.path().join("ws"), "aarch64-apple-darwin", &NO_LOCK)
            .unwrap_err();
        assert!(err.to_string().contains("build = true"), "{err}");
    }

    #[test]
    fn reads_profiles() {
        let dir = write_tree(&[
            (
                "Cargo.toml",
                r#"
[package]
name = "app"
version = "0.1.0"
edition = "2021"

[profile.release]
opt-level = 2
lto = "thin"
panic = "abort"
codegen-units = 4
overflow-checks = false
"#,
            ),
            ("src/main.rs", "fn main() {}"),
        ]);
        let model =
            import_cargo_workspace(&dir.path().join("ws"), "aarch64-apple-darwin", &NO_LOCK)
                .unwrap();
        let release = model.profiles.get("release").unwrap();
        assert_eq!(release.opt_level, "2");
        assert_eq!(release.lto, Lto::Thin);
        assert_eq!(release.panic, PanicStrategy::Abort);
        assert_eq!(release.codegen_units, Some(4));
        assert_eq!(release.overflow_checks, Some(false));
        // New parity keys map through.
        assert_eq!(release.debug_assertions, Some(false));
        assert_eq!(release.rpath, None);
    }

    #[test]
    fn filters_target_specific_dependencies() {
        let dir = write_tree(&[
            (
                "Cargo.toml",
                r#"
[package]
name = "app"
version = "0.1.0"
edition = "2021"

[dependencies]
common = { path = "../common" }

[target.'cfg(unix)'.dependencies]
unix-only = { path = "../unix-only" }

[target.'cfg(windows)'.dependencies]
win-only = { path = "../win-only" }
"#,
            ),
            ("src/main.rs", "fn main() {}"),
            (
                "../common/Cargo.toml",
                "[package]\nname = \"common\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            ),
            ("../common/src/lib.rs", ""),
            (
                "../unix-only/Cargo.toml",
                "[package]\nname = \"unix-only\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            ),
            ("../unix-only/src/lib.rs", ""),
            (
                "../win-only/Cargo.toml",
                "[package]\nname = \"win-only\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            ),
            ("../win-only/src/lib.rs", ""),
        ]);
        let model =
            import_cargo_workspace(&dir.path().join("ws"), "aarch64-apple-darwin", &NO_LOCK)
                .unwrap();
        // Target-specific deps stay in the model with their target
        // recorded (cargo locks all targets); the backend filters them at
        // plan time.
        let app = model.packages.iter().find(|p| p.name == "app").unwrap();
        let names: Vec<&str> = app.deps.iter().map(|d| d.package.as_str()).collect();
        assert!(names.contains(&"common"));
        assert!(names.contains(&"unix-only"));
        assert!(names.contains(&"win-only"));
        let win = app.deps.iter().find(|d| d.package == "win-only").unwrap();
        assert_eq!(win.target.as_deref(), Some("cfg(windows)"));
        assert_eq!(
            app.deps
                .iter()
                .find(|d| d.package == "unix-only")
                .unwrap()
                .target
                .as_deref(),
            Some("cfg(unix)")
        );

        // The Windows import keeps the same model shape (targets recorded).
        let model =
            import_cargo_workspace(&dir.path().join("ws"), "x86_64-pc-windows-msvc", &NO_LOCK)
                .unwrap();
        let app = model.packages.iter().find(|p| p.name == "app").unwrap();
        let names: Vec<&str> = app.deps.iter().map(|d| d.package.as_str()).collect();
        assert!(names.contains(&"unix-only"));
        assert!(names.contains(&"win-only"));
    }

    #[test]
    fn dep_table_target_key_filters() {
        let dir = write_tree(&[
            (
                "Cargo.toml",
                r#"
[package]
name = "app"
version = "0.1.0"
edition = "2021"

[dependencies]
unix-only = { path = "../unix-only", target = "cfg(unix)" }
win-only = { path = "../win-only", target = "cfg(windows)" }
"#,
            ),
            ("src/main.rs", "fn main() {}"),
            (
                "../unix-only/Cargo.toml",
                "[package]\nname = \"unix-only\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            ),
            ("../unix-only/src/lib.rs", ""),
            (
                "../win-only/Cargo.toml",
                "[package]\nname = \"win-only\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            ),
            ("../win-only/src/lib.rs", ""),
        ]);
        let model =
            import_cargo_workspace(&dir.path().join("ws"), "aarch64-apple-darwin", &NO_LOCK)
                .unwrap();
        // Both target-keyed deps are in the model with their targets; the
        // backend filters at plan time.
        let app = model.packages.iter().find(|p| p.name == "app").unwrap();
        assert_eq!(app.deps.len(), 2);
        assert_eq!(app.deps[0].package, "unix-only");
        assert_eq!(app.deps[1].package, "win-only");
        assert_eq!(app.deps[1].target.as_deref(), Some("cfg(windows)"));
    }

    #[test]
    fn unknown_cfg_predicates_evaluate_false() {
        let dir = write_tree(&[
            (
                "Cargo.toml",
                r#"
[package]
name = "app"
version = "0.1.0"
edition = "2021"

[target.'cfg(target_unknown = "x")'.dependencies]
foo = { path = "../foo" }
"#,
            ),
            ("src/main.rs", "fn main() {}"),
            (
                "../foo/Cargo.toml",
                "[package]\nname = \"foo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            ),
            ("../foo/src/lib.rs", ""),
        ]);
        // An unknown predicate is never set by the compiler: the import
        // records it as the dep's target (no error); the backend filters
        // it out at plan time.
        let model =
            import_cargo_workspace(&dir.path().join("ws"), "aarch64-apple-darwin", &NO_LOCK)
                .unwrap();
        let app = model.packages.iter().find(|p| p.name == "app").unwrap();
        assert_eq!(app.deps.len(), 1);
        assert_eq!(
            app.deps[0].target.as_deref(),
            Some("cfg(target_unknown = \"x\")")
        );
    }

    #[test]
    fn rejects_invalid_strip_values() {
        let dir = write_tree(&[
            (
                "Cargo.toml",
                r#"
[package]
name = "app"
version = "0.1.0"
edition = "2021"

[profile.release]
strip = "everything"
"#,
            ),
            ("src/main.rs", "fn main() {}"),
        ]);
        let err = import_cargo_workspace(&dir.path().join("ws"), "aarch64-apple-darwin", &NO_LOCK)
            .unwrap_err();
        assert!(err.to_string().contains("strip"), "{err}");
    }
}
