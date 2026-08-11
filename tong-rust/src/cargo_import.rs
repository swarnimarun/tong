//! `Cargo.toml` import mode (PLAN.md section 8.1).
//!
//! Cargo manifests are translated into the same [`RustModel`] the native
//! `Tong.toml` targets produce; Cargo is never invoked during a Tong build.
//! Path and workspace dependencies import directly; registry dependencies
//! resolve through `Tong.lock` + the source store (the driver's
//! [`LockedSourceProvider`]). Git dependencies are not yet supported and
//! fail with a targeted diagnostic.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::model::{
    BinTarget, Dep, Edition, LibTarget, Lto, Package, PackageId, PanicStrategy, ProfileSpec,
    RegistryEdge, ResolverVersion, RustModel, SourceId, TestTarget, crate_name, lib_crate_name,
    source_rel_path,
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
    /// `[patch.<source>]`: replacement packages for registry sources
    /// (top-level table; workspace-root only).
    #[serde(default)]
    patch: BTreeMap<String, BTreeMap<String, DepValue>>,
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
    /// Cargo resolver version: `"1"`, `"2"`, or `"3"`.
    #[serde(default)]
    resolver: Option<String>,
    /// Disable auto-discovery of `src/lib.rs`.
    #[serde(default)]
    autolib: Option<bool>,
    /// Disable auto-discovery of `src/bin/*`.
    #[serde(default)]
    autobins: Option<bool>,
    /// Disable auto-discovery of `examples/*`.
    #[serde(default)]
    autoexamples: Option<bool>,
    /// Disable auto-discovery of `tests/*`.
    #[serde(default)]
    autotests: Option<bool>,
    /// Disable auto-discovery of `benches/*`.
    #[serde(default)]
    autobenches: Option<bool>,
    /// Native library name this package links (Cargo: one package per
    /// value; the build script exports `DEP_<LINKS>_<KEY>` metadata).
    links: Option<String>,
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
    /// Members built by default; absent = all members.
    #[serde(default)]
    default_members: Vec<String>,
    /// Member paths excluded from the workspace.
    #[serde(default)]
    exclude: Vec<String>,
    #[serde(default)]
    dependencies: BTreeMap<String, DepValue>,
    /// `[workspace.package]` — defaults inherited by members.
    #[serde(default)]
    package: Option<CargoWorkspacePackage>,
    /// Cargo resolver version: `"1"`, `"2"`, or `"3"`.
    #[serde(default)]
    resolver: Option<String>,
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
    /// A table: path/version/workspace/git deps (and renames).
    Table {
        version: Option<String>,
        path: Option<String>,
        /// `git = "<url>"` — fixed git dependency.
        git: Option<String>,
        /// `rev = "<commit-ish>"` — exact lock resolution.
        rev: Option<String>,
        /// `tag = "<tag>"` — resolved only by `tong lock`.
        tag: Option<String>,
        /// `branch = "<branch>"` — resolved only by `tong lock`.
        branch: Option<String>,
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
    #[serde(default)]
    required_features: Option<Vec<String>>,
}

/// `[[test]]` / `[[bench]]` entry. Cargo defaults: path is
/// `tests/<name>.rs` / `benches/<name>.rs`, harness defaults to true.
#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
struct CargoTest {
    name: Option<String>,
    path: Option<String>,
    harness: Option<bool>,
    #[serde(default)]
    required_features: Option<Vec<String>>,
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
    crate_type: Vec<String>,
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
    /// Base profile to inherit from (`dev` by default).
    inherits: Option<String>,
    opt_level: Option<OptLevelValue>,
    debug: Option<DebugValue>,
    lto: Option<LtoValue>,
    panic: Option<String>,
    codegen_units: Option<u32>,
    overflow_checks: Option<bool>,
    debug_assertions: Option<bool>,
    strip: Option<String>,
    rpath: Option<bool>,
    /// `none` | `unpacked` | `packed`.
    split_debuginfo: Option<String>,
    /// Requested incremental compilation; Tong keeps it disabled for
    /// cacheability (one structured divergence note).
    incremental: Option<bool>,
    /// Alternative codegen backend (unsupported: changes compilation).
    codegen_backend: Option<String>,
    /// Build-script override table (unsupported).
    build_override: Option<toml::Value>,
    /// Per-package overrides (`[profile.<name>.package.<spec>]`).
    package: Option<BTreeMap<String, CargoProfile>>,
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
    Str(String),
}

#[derive(Deserialize, Clone)]
#[serde(untagged)]
enum LtoValue {
    Bool(bool),
    Str(String),
}

// `.cargo/config` / `.cargo/config.toml` subset. Only the root config is
// read — user-home and ancestor configuration is not a declared workspace
// input. Settings that would alter resolution or compilation but remain
// unsupported are targeted errors, never silently ignored.
#[derive(Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
struct CargoConfig {
    build: Option<CargoBuild>,
    env: Option<BTreeMap<String, EnvValue>>,
    /// `[target.'<triple>']` / `[target.'cfg(...)']` tables.
    #[serde(default)]
    target: BTreeMap<String, CargoTargetConfig>,
    /// Alternate registries — unsupported.
    #[serde(default)]
    registries: BTreeMap<String, toml::Value>,
    /// Source replacement — unsupported.
    #[serde(default)]
    source: BTreeMap<String, toml::Value>,
    /// Dependency aliases — unsupported.
    #[serde(default)]
    alias: BTreeMap<String, toml::Value>,
    /// `[net]` (offline, retry, …) — unsupported.
    #[serde(default)]
    net: Option<toml::Value>,
    /// `[http]` (proxies, auth) — unsupported.
    #[serde(default)]
    http: Option<toml::Value>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
struct CargoBuild {
    rustflags: Option<RustflagsValue>,
    /// Default target triple — cross-compilation is unsupported.
    target: Option<String>,
}

/// rustflags may be a list or a single string.
#[derive(Deserialize, Clone)]
#[serde(untagged)]
enum RustflagsValue {
    List(Vec<String>),
    Single(String),
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
struct CargoTargetConfig {
    rustflags: Option<RustflagsValue>,
    /// Linker override (`-C linker=<path>`).
    linker: Option<String>,
}

#[derive(Deserialize, Clone)]
#[serde(untagged)]
enum EnvValue {
    Plain(String),
    Table {
        value: String,
        /// Force the variable even when already set (we always apply it).
        #[serde(default)]
        force: Option<bool>,
        /// Relative paths resolve against the config dir (we apply the
        /// value as declared).
        #[serde(default)]
        relative: Option<bool>,
    },
}

/// A locked package resolved for one registry edge: its exact identity and
/// the extracted source directory it imports from.
#[derive(Clone, Debug)]
pub struct LockedSource {
    /// Exact locked identity (name + version + source).
    pub id: crate::model::PackageId,
    /// Extracted source directory (materialized from the store).
    pub source_dir: PathBuf,
    /// Whether the source identity propagates to every package inside the
    /// checkout (git dependencies: all packages from one repository share
    /// the `git+<url>#<commit>` identity). Registry checkouts never
    /// propagate.
    pub propagate_source: bool,
}

/// A source of locked registry packages: exact edge resolution and
/// extracted source directories.
///
/// Implemented by the driver over `Tong.lock` + the source store; `tong
/// lock` uses a collecting provider that records edges instead of
/// resolving them.
pub trait LockedSourceProvider {
    /// Resolves one registry dep edge to its locked package. `Ok(None)`
    /// means the edge is not resolved: collecting mode (`tong lock`) —
    /// the edge is recorded instead — or an inactive optional edge absent
    /// from the locked graph. An `Err` is a targeted diagnostic (missing
    /// lock, missing entry, or a lockfile out of date).
    fn locked_package(&self, edge: &RegistryEdge)
    -> Result<Option<LockedSource>, CargoImportError>;

    /// Whether this provider is in collecting mode (`tong lock`): every
    /// registry edge is recorded for the lockfile even when it cannot be
    /// resolved yet, and the model keeps provisional edges so feature
    /// resolution can activate `dep:` references. Lock-backed providers
    /// leave this false: unresolved edges are dropped from the model.
    fn collecting(&self) -> bool {
        false
    }
}

/// The crate names replaced by the root manifest's `[patch]` tables.
/// Registry requirements on these names resolve to the patched local
/// package (serde's root patches `serde`/`serde_core`/`serde_derive` to
/// the workspace members).
pub fn cargo_patch_names(workspace_root: &Path) -> Result<Vec<String>, CargoImportError> {
    let manifest = read_manifest(workspace_root)?;
    let mut names: Vec<String> = manifest
        .patch
        .get("crates-io")
        .map(|entries| entries.keys().cloned().collect())
        .unwrap_or_default();
    names.sort();
    names.dedup();
    Ok(names)
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
    encoded_rustflags: Option<&str>,
) -> Result<RustModel, CargoImportError> {
    let root_manifest = read_manifest(workspace_root)?;

    // Workspace inheritance: [workspace.dependencies], [workspace.package],
    // and top-level [patch.<source>] (root-only, crates-io source).
    let mut inherited = Inherited::default();
    let mut patches: BTreeMap<String, DepValue> = BTreeMap::new();
    if let Some(workspace) = &root_manifest.workspace {
        inherited.deps.extend(workspace.dependencies.clone());
        inherited.package = workspace.package.clone();
    }
    if let Some(patch) = root_manifest.patch.get("crates-io").or_else(|| {
        root_manifest
            .patch
            .iter()
            .find(|(source, _)| source.starts_with("registry+"))
            .map(|(_, entries)| entries)
    }) {
        patches.extend(patch.clone());
    }
    for source in root_manifest.patch.keys() {
        if source != "crates-io" && !source.starts_with("registry+") {
            return Err(CargoImportError::Unsupported(format!(
                "[patch.{source}] sources other than the default registry \
                 are not supported"
            )));
        }
    }

    // Resolver selection (Cargo semantics): an explicit `resolver` on the
    // root package or workspace wins; otherwise the root package's
    // effective edition picks the default (2021+ → 2, 2024 → 3, older → 1).
    let root_edition = match &root_manifest.package {
        Some(package) => resolve_field(
            &package.edition,
            inherited.package.as_ref(),
            &package.name,
            "edition",
            workspace_root,
            "2015",
        )?,
        None => "2015".to_owned(),
    };
    let resolver_text = root_manifest
        .workspace
        .as_ref()
        .and_then(|workspace| workspace.resolver.as_deref())
        .or_else(|| {
            root_manifest
                .package
                .as_ref()
                .and_then(|package| package.resolver.as_deref())
        });
    let resolver = ResolverVersion::from_manifest(resolver_text, parse_edition(&root_edition)?)
        .map_err(CargoImportError::Unsupported)?;

    let members: Vec<PathBuf> = match &root_manifest.workspace {
        Some(workspace) if !workspace.members.is_empty() => expand_members(
            workspace_root,
            &workspace.members,
            &workspace.exclude,
            &root_manifest.package,
        )?,
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
    // `[workspace] default_members` narrows the default build selection.
    let default_members: Vec<PathBuf> = match &root_manifest.workspace {
        Some(workspace) if !workspace.default_members.is_empty() => expand_members(
            workspace_root,
            &workspace.default_members,
            &workspace.exclude,
            &root_manifest.package,
        )?,
        _ => Vec::new(),
    };

    let mut model = RustModel {
        resolver,
        ..Default::default()
    };
    // Canonical package dir → imported package. Path dependencies outside
    // the workspace are imported recursively (Cargo semantics), so the
    // graph is closed over every path dep, not just the members.
    let mut packages: BTreeMap<PathBuf, Package> = BTreeMap::new();
    // (canonical dir, reached via a dev-dependency edge). Cargo permits
    // cycles that contain a dev edge anywhere in the cycle, not only on
    // the closing edge.
    let mut visiting: Vec<(PathBuf, bool)> = Vec::new();
    let mut member_ids: Vec<PackageId> = Vec::new();
    for member in &members {
        let id = import_package(
            member,
            true,
            false,
            None,
            &mut packages,
            &mut visiting,
            &inherited,
            workspace_root,
            host_triple,
            sources,
            &patches,
            &members,
        )?;
        member_ids.push(id);
    }
    member_ids.sort();

    model.packages = packages.into_values().collect();
    model.members = member_ids;
    // Cargo: at most one package per `links` value.
    let mut links_owners: BTreeMap<&str, &str> = BTreeMap::new();
    for pkg in &model.packages {
        if let Some(links) = &pkg.links
            && let Some(previous) = links_owners.insert(links, &pkg.name)
        {
            return Err(CargoImportError::Unsupported(format!(
                "the `links` key {links:?} is declared by both {previous} and {}; \
                 Cargo requires exactly one package per links value",
                pkg.name
            )));
        }
    }
    if !default_members.is_empty() {
        let default_canonical: Vec<PathBuf> = default_members
            .iter()
            .map(|dir| fs::canonicalize(dir).unwrap_or_else(|_| dir.clone()))
            .collect();
        model.default_members = model
            .packages
            .iter()
            .filter(|pkg| {
                let canonical = fs::canonicalize(&pkg.dir).unwrap_or_else(|_| pkg.dir.clone());
                default_canonical.contains(&canonical)
            })
            .map(|pkg| pkg.id.clone())
            .collect();
    }

    // Profiles from the workspace root manifest (Cargo: [profile.*] tables).
    let (profiles, package_profiles) = resolve_profiles(&root_manifest.profile)?;
    model.profiles = profiles;
    model.package_profiles = package_profiles;
    model
        .profiles
        .entry("dev".to_owned())
        .or_insert_with(ProfileSpec::dev);
    model
        .profiles
        .entry("release".to_owned())
        .or_insert_with(ProfileSpec::release);

    // `.cargo/config` / `.cargo/config.toml`: rustflags (with Cargo's
    // precedence: `[target]` matching the host overrides `[build]`
    // overrides `CARGO_ENCODED_RUSTFLAGS`) and `[env]`. Unsupported
    // config that would alter resolution or compilation is a targeted
    // error.
    let config = load_config(workspace_root)?;
    if !config.registries.is_empty() {
        return Err(CargoImportError::Unsupported(
            "alternate registries in .cargo/config are not supported".to_owned(),
        ));
    }
    if !config.source.is_empty() {
        return Err(CargoImportError::Unsupported(
            "source replacement in .cargo/config is not supported".to_owned(),
        ));
    }
    // `[alias]` is a cargo CLI convenience (`cargo xtask` →
    // `cargo run -p wgpu-xtask --`); it never alters the build graph, so
    // it is parsed and ignored like other benign metadata.
    let _ = &config.alias;
    if config.net.is_some() || config.http.is_some() {
        return Err(CargoImportError::Unsupported(
            "[net] and [http] settings in .cargo/config are not supported".to_owned(),
        ));
    }
    if let Some(build) = &config.build
        && build.target.is_some()
    {
        return Err(CargoImportError::Unsupported(
            "[build] target (a default target triple) is not supported".to_owned(),
        ));
    }
    let mut global_rustflags: Vec<String> = Vec::new();
    if let Some(encoded) = encoded_rustflags {
        global_rustflags = encoded.split(' ').map(str::to_owned).collect();
    }
    if let Some(build) = &config.build
        && let Some(flags) = &build.rustflags
    {
        global_rustflags = flags.as_vec();
    }
    for (key, table) in &config.target {
        let matching = target_matches(key, host_triple, &format!("target config {key:?}"))?;
        if !matching {
            continue;
        }
        if let Some(flags) = &table.rustflags {
            global_rustflags = flags.as_vec();
        }
        if let Some(linker) = &table.linker {
            global_rustflags.push("-C".to_owned());
            global_rustflags.push(format!("linker={linker}"));
        }
    }
    model.global_rustflags = global_rustflags;
    if let Some(env) = &config.env {
        for (key, value) in env {
            // `force` and `relative` are parsed and applied as declared:
            // the variable is always set (force is moot), and relative
            // paths are not resolved against the config dir.
            let (value, _force, _relative) = match value {
                EnvValue::Plain(v) => (v.clone(), false, false),
                EnvValue::Table {
                    value: v,
                    force,
                    relative,
                } => (v.clone(), force.unwrap_or(false), relative.unwrap_or(false)),
            };
            model.global_env.insert(key.clone(), value);
        }
    }

    Ok(model)
}

impl RustflagsValue {
    fn as_vec(&self) -> Vec<String> {
        match self {
            Self::List(items) => items.clone(),
            Self::Single(flag) => vec![flag.clone()],
        }
    }
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
    exclude: &[String],
    root_package: &Option<CargoPackage>,
) -> Result<Vec<PathBuf>, CargoImportError> {
    let mut out = Vec::new();
    // The root itself may also be a member (workspace root package).
    if root_package.is_some() {
        out.push(root.to_path_buf());
    }
    let excluded: Vec<PathBuf> = exclude
        .iter()
        .map(|entry| fs::canonicalize(root.join(entry)).unwrap_or_else(|_| root.join(entry)))
        .collect();
    let excluded = |dir: &Path| -> bool {
        let canonical = fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
        excluded.contains(&canonical)
    };
    for member in members {
        if member.ends_with('*') {
            // Cargo member globs support a single trailing `*` on the
            // last component: `crates/*` lists every crate under
            // `crates/`, and `axum-*` lists every root directory whose
            // name starts with `axum-`.
            let (dir, name_prefix) = if let Some(dir) = member.strip_suffix("/*") {
                (root.join(dir), String::new())
            } else {
                let prefix = &member[..member.len() - 1];
                match prefix.rfind('/') {
                    Some(slash) => (root.join(&prefix[..slash]), prefix[slash + 1..].to_owned()),
                    None => (root.to_path_buf(), prefix.to_owned()),
                }
            };
            let entries = fs::read_dir(&dir)
                .map_err(|err| CargoImportError::Io(dir.display().to_string(), err))?;
            let mut found = false;
            for entry in entries {
                let entry = entry.map_err(|err| CargoImportError::Io(String::new(), err))?;
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if !name.starts_with(&name_prefix) {
                    continue;
                }
                if entry
                    .file_type()
                    .map_err(|err| CargoImportError::Io(String::new(), err))?
                    .is_dir()
                    && entry.path().join("Cargo.toml").is_file()
                    && !excluded(&entry.path())
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
                if !excluded(&path) {
                    out.push(path);
                }
            } else {
                return Err(CargoImportError::Unsupported(format!(
                    "workspace member {member:?} has no Cargo.toml"
                )));
            }
        }
    }
    Ok(out)
}

/// How a package's identity is forced during import.
#[derive(Clone, Debug)]
enum ForcedIdentity {
    /// The exact locked identity: name and version must match the manifest
    /// (registry packages).
    Exact(crate::model::PackageId),
    /// Only the source is forced; name/version come from the manifest
    /// (git dependencies — the source propagates to every package in the
    /// checkout).
    Source(crate::model::SourceId),
}

/// Imports the package at `dir` (a workspace member or a path dependency)
/// into `packages`, recursing into its path dependencies. Returns the
/// package's exact identity. Cycles are rejected, matching Cargo.
///
/// `forced` overrides the derived identity: registry checkouts imported
/// from the lock keep the lock's exact `(name, version, source)`, and git
/// checkouts keep the git source (propagated to nested path dependencies).
#[allow(clippy::too_many_arguments)]
fn import_package(
    dir: &Path,
    is_member: bool,
    via_dev: bool,
    forced: Option<ForcedIdentity>,
    packages: &mut BTreeMap<PathBuf, Package>,
    visiting: &mut Vec<(PathBuf, bool)>,
    inherited: &Inherited,
    workspace_root: &Path,
    host_triple: &str,
    sources: &dyn LockedSourceProvider,
    patches: &BTreeMap<String, DepValue>,
    members: &[PathBuf],
) -> Result<PackageId, CargoImportError> {
    let canonical = fs::canonicalize(dir)
        .map_err(|err| CargoImportError::Io(dir.display().to_string(), err))?;
    if let Some(pkg) = packages.get(&canonical) {
        return Ok(pkg.id.clone());
    }
    // A package reached through a dependency traversal is still a member
    // when the members list names it (axum's normal dep axum-core is
    // imported before the member loop reaches it): dev-dependencies and
    // the Workspace source identity then apply.
    let is_member = is_member
        || members.iter().any(|member| {
            fs::canonicalize(member)
                .map(|m| m == canonical)
                .unwrap_or(false)
        });
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

    // Exact identity: registry/git checkouts take the locked identity;
    // workspace members and path deps derive it from the lexical path
    // relative to the workspace root (never the canonical absolute
    // path, so digests are host-independent).
    let id = match &forced {
        Some(ForcedIdentity::Exact(locked)) => {
            let parsed = semver::Version::parse(&version).ok();
            if locked.name != package.name || parsed.as_ref() != Some(&locked.version) {
                return Err(CargoImportError::Unsupported(format!(
                    "locked dependency {} v{} resolves to package {} v{} in {}, \
                     but the manifest declares {} v{version}; run `tong lock`",
                    locked.name,
                    locked.version,
                    package.name,
                    parsed
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "?".to_owned()),
                    canonical.display(),
                    package.name,
                )));
            }
            locked.clone()
        }
        Some(ForcedIdentity::Source(source)) => {
            let version = semver::Version::parse(&version).map_err(|err| {
                CargoImportError::Unsupported(format!(
                    "package {} in {} has invalid version {:?}: {err}",
                    package.name,
                    canonical.display(),
                    version
                ))
            })?;
            PackageId {
                name: package.name.clone(),
                version,
                source: source.clone(),
            }
        }
        None => {
            let version = semver::Version::parse(&version).map_err(|err| {
                CargoImportError::Unsupported(format!(
                    "package {} in {} has invalid version {:?}: {err}",
                    package.name,
                    canonical.display(),
                    version
                ))
            })?;
            let rel = source_rel_path(dir, workspace_root);
            let source = if is_member {
                SourceId::Workspace(rel)
            } else {
                SourceId::Path(rel)
            };
            PackageId {
                name: package.name.clone(),
                version,
                source,
            }
        }
    };

    // Cargo permits dependency cycles that contain a dev-dependency or
    // inactive optional edge anywhere in the cycle (e.g. axum and
    // axum-extra dev-depend on each other; tracing dev-depends on
    // tracing-mock which normal-depends back). The identity must match
    // what the real import will derive — in particular, a cycle-closing
    // edge back to a workspace member carries the Workspace source, not
    // Path.
    if visiting.iter().any(|(dir, _)| dir == &canonical) {
        let cycle_has_dev_edge = via_dev || visiting.iter().any(|(_, dev)| *dev);
        if cycle_has_dev_edge {
            let member = members.iter().any(|member| {
                fs::canonicalize(member)
                    .map(|m| m == canonical)
                    .unwrap_or(false)
            });
            let rel = source_rel_path(dir, workspace_root);
            let source = if member {
                SourceId::Workspace(rel)
            } else {
                SourceId::Path(rel)
            };
            let id = match &forced {
                Some(ForcedIdentity::Exact(locked)) => locked.clone(),
                Some(ForcedIdentity::Source(source)) => PackageId {
                    name: package.name.clone(),
                    version: id.version,
                    source: source.clone(),
                },
                None => PackageId {
                    name: package.name.clone(),
                    version: id.version,
                    source,
                },
            };
            return Ok(id);
        }
        return Err(CargoImportError::Unsupported(format!(
            "cyclic path dependency involving {}",
            dir.display()
        )));
    }
    visiting.push((canonical.clone(), via_dev));

    let result = (|| {
        let mut pkg = Package {
            id: id.clone(),
            name: package.name.clone(),
            dir: canonical.clone(),
            version,
            edition: parse_edition(&edition)?,
            lib: None,
            bins: Vec::new(),
            examples: Vec::new(),
            tests: Vec::new(),
            build_script: None,
            links: package.links.clone(),
            deps: Vec::new(),
            optional_anywhere: BTreeSet::new(),
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
        } else if lib_present && package.autolib.unwrap_or(true) {
            pkg.lib = Some(LibTarget {
                name: None,
                crate_types: Vec::new(),
                proc_macro: false,
                path: lib_path,
            });
        }

        // Binaries: explicit [[bin]] or auto-detected src/main.rs.
        if manifest.bin.is_empty()
            && package.autobins.unwrap_or(true)
            && pkg.dir.join("src/main.rs").is_file()
        {
            pkg.bins.push(BinTarget {
                name: package.name.clone(),
                crate_name: crate_name(&package.name),
                path: PathBuf::from("src/main.rs"),
                required_features: Vec::new(),
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
            pkg.bins.push(BinTarget {
                name: name.clone(),
                crate_name: crate_name(&name),
                path,
                required_features: bin.required_features.clone().unwrap_or_default(),
            });
        }

        // Examples: [[example]] entries whose source exists, plus
        // auto-discovered `examples/*.rs` (Cargo conventions; tong plans
        // example compiles in test/`--all-targets` builds).
        let mut example_entries: Vec<(String, PathBuf, Vec<String>)> = Vec::new();
        for example in &manifest.example {
            let name = example.name.clone().unwrap_or_else(|| {
                example
                    .path
                    .as_ref()
                    .and_then(|p| Path::new(p).file_stem())
                    .and_then(|s| s.to_str())
                    .unwrap_or(&package.name)
                    .to_owned()
            });
            let path = example
                .path
                .clone()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(format!("examples/{name}.rs")));
            if pkg.dir.join(&path).is_file() {
                example_entries.push((
                    name,
                    path,
                    example.required_features.clone().unwrap_or_default(),
                ));
            }
        }
        if package.autoexamples.unwrap_or(true)
            && let Ok(entries) = fs::read_dir(pkg.dir.join("examples"))
        {
            let mut names: Vec<String> =
                example_entries.iter().map(|(n, _, _)| n.clone()).collect();
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|ext| ext == "rs") {
                    let name = path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or_default()
                        .to_owned();
                    if !names.contains(&name) {
                        example_entries.push((
                            name.clone(),
                            PathBuf::from(format!("examples/{name}.rs")),
                            Vec::new(),
                        ));
                        names.push(name);
                    }
                }
            }
        }
        for (name, path, required_features) in example_entries {
            pkg.examples.push(crate::model::ExampleTarget {
                name: name.clone(),
                crate_name: crate_name(&name),
                path,
                required_features,
            });
        }

        // Test targets: [[test]] / [[bench]] entries whose source exists
        // (Cargo drops targets without source files), auto-discovered
        // `tests/*.rs` / `benches/*.rs`, plus the auto-derived lib unit
        // test. [[example]] targets are handled above.
        for (entry, default_dir, kind, auto) in [
            (
                &manifest.test,
                "tests",
                "test",
                package.autotests.unwrap_or(true),
            ),
            (
                &manifest.bench,
                "benches",
                "bench",
                package.autobenches.unwrap_or(true),
            ),
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
                    bench: kind == "bench",
                    harness: target.harness.unwrap_or(true),
                    doc: false,
                    cache_test_result: false,
                    required_features: target.required_features.clone().unwrap_or_default(),
                });
            }
            // Auto-discovery: `tests/*.rs` / `benches/*.rs` when no
            // explicit entries exist (Cargo conventions).
            if entry.is_empty()
                && auto
                && let Ok(entries) = fs::read_dir(pkg.dir.join(default_dir))
            {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().is_some_and(|ext| ext == "rs") {
                        let name = path
                            .file_stem()
                            .and_then(|s| s.to_str())
                            .unwrap_or_default()
                            .to_owned();
                        pkg.tests.push(TestTarget {
                            name: name.clone(),
                            path: PathBuf::from(format!("{default_dir}/{name}.rs")),
                            bench: kind == "bench",
                            harness: true,
                            doc: false,
                            cache_test_result: false,
                            required_features: Vec::new(),
                        });
                    }
                }
            }
        }
        // The package's own library unit test (Cargo: `--test` on the lib).
        if let Some(lib) = &pkg.lib {
            pkg.tests.push(TestTarget {
                name: lib_crate_name(&pkg),
                path: lib.path.clone(),
                bench: false,
                harness: true,
                doc: false,
                cache_test_result: false,
                required_features: Vec::new(),
            });
        }

        // Dependencies (path and workspace-inherited only), with
        // target-specific tables merged in for matching targets. Direct
        // paths resolve relative to this manifest; inherited paths resolve
        // relative to the workspace root.
        let (dependencies, build_dependencies, dev_dependencies, optional_anywhere) =
            merge_target_tables(&manifest, host_triple)?;
        pkg.optional_anywhere = optional_anywhere;
        let resolved_deps = resolve_deps(
            &dependencies,
            &pkg,
            &canonical,
            workspace_root,
            &inherited.deps,
            patches,
            host_triple,
            sources,
        )?;
        let resolved_build_deps = resolve_deps(
            &build_dependencies,
            &pkg,
            &canonical,
            workspace_root,
            &inherited.deps,
            patches,
            host_triple,
            sources,
        )?;
        let resolved_dev_deps = if is_member {
            resolve_deps(
                &dev_dependencies,
                &pkg,
                &canonical,
                workspace_root,
                &inherited.deps,
                patches,
                host_triple,
                sources,
            )?
        } else {
            Vec::new()
        };
        let mut deps = Vec::new();
        let mut build_deps = Vec::new();
        let mut dev_deps = Vec::new();
        for (resolved, target, via_dev) in [
            (resolved_deps, &mut deps, false),
            (resolved_build_deps, &mut build_deps, false),
            (resolved_dev_deps, &mut dev_deps, true),
        ] {
            let _ = &via_dev;
            for dep in resolved {
                let package_id = match dep.path {
                    Some(path) => {
                        // A path dependency of a git checkout inherits the
                        // checkout's source identity (all packages from one
                        // repository share the git source).
                        let forced = match &forced {
                            Some(ForcedIdentity::Source(source)) => {
                                Some(ForcedIdentity::Source(source.clone()))
                            }
                            _ => None,
                        };
                        let allow_cycle = via_dev || dep.optional;
                        let imported = import_package(
                            &path,
                            false,
                            allow_cycle,
                            forced,
                            packages,
                            visiting,
                            &inherited,
                            workspace_root,
                            host_triple,
                            sources,
                            patches,
                            members,
                        )?;
                        if imported.name != dep.package {
                            return Err(CargoImportError::Unsupported(format!(
                                "path dependency {} = {{ path = {:?} }} resolves to package \
                                 {:?}, not {:?}",
                                dep.extern_name.replace('_', "-"),
                                path.display(),
                                imported.name,
                                dep.package
                            )));
                        }
                        imported
                    }
                    None => {
                        // Registry/git dependency: resolved through the
                        // lockfile-backed source provider (which validated
                        // the requirement); in collecting mode the edge was
                        // recorded and nothing is imported.
                        let Some(locked) = dep.locked else {
                            // Collecting mode (`tong lock`): the provider
                            // recorded the RegistryEdge; the model keeps a
                            // provisional edge so feature resolution can
                            // activate `dep:` references (e.g. axum's
                            // `tracing = ["dep:tracing"]`). The exact
                            // identity is substituted once the lock
                            // resolves the version.
                            target.push(Dep {
                                extern_name: dep.extern_name,
                                package: PackageId {
                                    name: dep.package.clone(),
                                    version: semver::Version::new(0, 0, 0),
                                    source: SourceId::Registry(String::new()),
                                },
                                optional: dep.optional,
                                default_features: dep.default_features,
                                features: dep.features,
                                target: dep.target,
                            });
                            continue;
                        };
                        let forced = if locked.propagate_source {
                            ForcedIdentity::Source(locked.id.source.clone())
                        } else {
                            ForcedIdentity::Exact(locked.id.clone())
                        };
                        import_package(
                            &locked.source_dir,
                            false,
                            via_dev || dep.optional,
                            Some(forced),
                            packages,
                            visiting,
                            &inherited,
                            workspace_root,
                            host_triple,
                            sources,
                            patches,
                            members,
                        )?
                    }
                };
                target.push(Dep {
                    extern_name: dep.extern_name,
                    package: package_id,
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

        let id = pkg.id.clone();
        packages.insert(canonical, pkg);
        Ok(id)
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

/// A resolved dependency: the crate name used at the use site, the
/// declared package name it refers to, and — for path dependencies — the
/// package directory; registry dependencies carry their resolved locked
/// identity.
struct ResolvedDep {
    extern_name: String,
    package: String,
    path: Option<PathBuf>,
    optional: bool,
    default_features: bool,
    features: Vec<String>,
    target: Option<String>,
    locked: Option<LockedSource>,
}

#[allow(clippy::too_many_arguments)]
fn resolve_deps(
    deps: &[(String, DepValue)],
    parent: &Package,
    member: &Path,
    workspace_root: &Path,
    inherited: &BTreeMap<String, DepValue>,
    patches: &BTreeMap<String, DepValue>,
    _host_triple: &str,
    sources: &dyn LockedSourceProvider,
) -> Result<Vec<ResolvedDep>, CargoImportError> {
    let mut out = Vec::new();
    for (name, value) in deps {
        // The effective table: the member's own table merged over the
        // inherited `[workspace.dependencies]` table (Cargo semantics:
        // features concatenate, other keys override).
        let (path, package, optional, default_features, features, target, locked) = match value {
            DepValue::Version(version) => {
                // A `[patch]` entry replaces the registry package (path or
                // git patches only).
                if let Some(patch) = patches.get(name) {
                    let patched = apply_patch(
                        name,
                        patch,
                        parent,
                        member,
                        workspace_root,
                        inherited,
                        sources,
                    )?;
                    let (path, package, optional, default_features, features, target, locked) =
                        patched;
                    (
                        path,
                        package,
                        optional,
                        default_features,
                        features,
                        target,
                        locked,
                    )
                } else {
                    // Registry dependency: resolved through the lockfile.
                    let edge = RegistryEdge {
                        parent: parent.id.clone(),
                        extern_name: name.replace('-', "_"),
                        package: name.clone(),
                        req: version.clone(),
                        git: None,
                        optional: false,
                        default_features: true,
                        features: Vec::new(),
                    };
                    let locked = sources.locked_package(&edge)?;
                    (None, name.clone(), false, true, Vec::new(), None, locked)
                }
            }
            DepValue::Table {
                path,
                version,
                git,
                rev,
                tag,
                branch,
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
                let package_name = package.clone().unwrap_or_else(|| name.clone());
                if git.is_none()
                    && path.is_none()
                    && *workspace != Some(true)
                    && version.is_some()
                    && let Some(patch) = patches.get(&package_name)
                {
                    // `[patch]` selects the replacement source, while the
                    // dependency declaration still controls edge features,
                    // optionality, and target conditions.
                    let (path, package, _, _, _, _, locked) = apply_patch(
                        &package_name,
                        patch,
                        parent,
                        member,
                        workspace_root,
                        inherited,
                        sources,
                    )?;
                    (
                        path,
                        package,
                        optional,
                        default_features,
                        features,
                        target.clone(),
                        locked,
                    )
                } else if let Some(git) = git {
                    // A git dependency: fixed-revision source, never a
                    // registry or path dep. Conflicting combinations are
                    // targeted errors.
                    if path.is_some() {
                        return Err(CargoImportError::Unsupported(format!(
                            "dependency {name:?} combines `git` with `path`; \
                             Cargo requires exactly one source kind"
                        )));
                    }
                    if version.is_some() || *workspace == Some(true) {
                        return Err(CargoImportError::Unsupported(format!(
                            "dependency {name:?} combines `git` with {}; \
                             Cargo requires exactly one source kind",
                            if version.is_some() {
                                "`version` (registry)"
                            } else {
                                "`workspace = true`"
                            }
                        )));
                    }
                    let selectors = [&rev, &tag, &branch]
                        .into_iter()
                        .filter(|selector| selector.is_some())
                        .count();
                    if selectors > 1 {
                        return Err(CargoImportError::Unsupported(format!(
                            "dependency {name:?} sets more than one of `rev`, `tag`, \
                             and `branch`; Cargo requires exactly one"
                        )));
                    }
                    let selector = crate::model::GitSelector {
                        url: crate::model::GitSelector::canonical_url(git),
                        rev: rev.clone(),
                        tag: tag.clone(),
                        branch: branch.clone(),
                    };
                    let edge = RegistryEdge {
                        parent: parent.id.clone(),
                        extern_name: name.replace('-', "_"),
                        package: package.clone().unwrap_or_else(|| name.clone()),
                        req: "*".to_owned(),
                        git: Some(selector),
                        optional,
                        default_features,
                        features: features.clone(),
                    };
                    let locked = sources.locked_package(&edge)?;
                    (
                        None,
                        package.clone().unwrap_or_else(|| name.clone()),
                        optional,
                        default_features,
                        features,
                        target.clone(),
                        locked,
                    )
                } else if let Some(path) = path {
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
                            if let Some(patch) = patches.get(name) {
                                let (path, package, _, _, _, _, locked) = apply_patch(
                                    name,
                                    patch,
                                    parent,
                                    member,
                                    workspace_root,
                                    inherited,
                                    sources,
                                )?;
                                (
                                    path,
                                    package,
                                    optional,
                                    default_features,
                                    features,
                                    target.clone(),
                                    locked,
                                )
                            } else {
                                let edge = RegistryEdge {
                                    parent: parent.id.clone(),
                                    extern_name: name.replace('-', "_"),
                                    package: name.clone(),
                                    req: version.clone(),
                                    git: None,
                                    optional,
                                    default_features,
                                    features: features.clone(),
                                };
                                let locked = sources.locked_package(&edge)?;
                                (
                                    None,
                                    name.clone(),
                                    optional,
                                    default_features,
                                    features,
                                    target.clone(),
                                    locked,
                                )
                            }
                        }
                        Some(DepValue::Table {
                            version: Some(version),
                            package: inherited_package,
                            optional: inherited_optional,
                            default_features: inherited_default_features,
                            features: inherited_features,
                            ..
                        }) => {
                            // The workspace's `default-features = false`
                            // applies to the inherited edge (cargo: serde's
                            // `[workspace.dependencies] syn =
                            // { version = "3", default-features = false }`
                            // must not activate syn's defaults).
                            let default_features =
                                default_features && inherited_default_features.unwrap_or(true);
                            let optional = optional || inherited_optional.unwrap_or(false);
                            let mut merged_features =
                                inherited_features.clone().unwrap_or_default();
                            for feature in &features {
                                if !merged_features.contains(feature) {
                                    merged_features.push(feature.clone());
                                }
                            }
                            let package = package
                                .clone()
                                .or_else(|| inherited_package.clone())
                                .unwrap_or_else(|| name.clone());
                            if let Some(patch) = patches.get(&package) {
                                let (path, package, _, _, _, _, locked) = apply_patch(
                                    &package,
                                    patch,
                                    parent,
                                    member,
                                    workspace_root,
                                    inherited,
                                    sources,
                                )?;
                                (
                                    path,
                                    package,
                                    optional,
                                    default_features,
                                    merged_features,
                                    target.clone(),
                                    locked,
                                )
                            } else {
                                let edge = RegistryEdge {
                                    parent: parent.id.clone(),
                                    extern_name: name.replace('-', "_"),
                                    package: package.clone(),
                                    req: version.clone(),
                                    git: None,
                                    optional,
                                    default_features,
                                    features: merged_features.clone(),
                                };
                                let locked = sources.locked_package(&edge)?;
                                (
                                    None,
                                    package,
                                    optional,
                                    default_features,
                                    merged_features,
                                    target.clone(),
                                    locked,
                                )
                            }
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
                        parent: parent.id.clone(),
                        extern_name: name.replace('-', "_"),
                        package: package.clone().unwrap_or_else(|| name.clone()),
                        req: version.clone(),
                        git: None,
                        optional,
                        default_features,
                        features: features.clone(),
                    };
                    let locked = sources.locked_package(&edge)?;
                    (
                        None,
                        package.clone().unwrap_or_else(|| name.clone()),
                        optional,
                        default_features,
                        features,
                        target.clone(),
                        locked,
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
                locked: None,
            });
        } else if let Some(locked) = locked {
            out.push(ResolvedDep {
                extern_name: name.replace('-', "_"),
                package,
                path: None,
                optional,
                default_features,
                features,
                target,
                locked: Some(locked),
            });
        } else if sources.collecting() {
            // Collecting mode (`tong lock`): the edge was recorded by the
            // provider; return it unlocked so the caller keeps a
            // provisional edge in the model for feature resolution. In
            // lock-backed mode an unresolved edge is an inactive optional
            // dep absent from the locked feature graph — dropped here.
            out.push(ResolvedDep {
                extern_name: name.replace('-', "_"),
                package,
                path: None,
                optional,
                default_features,
                features,
                target,
                locked: None,
            });
        }
        // Lock-backed mode with an inactive optional edge: `locked` is
        // None — the edge is absent from the locked feature graph; nothing
        // is imported.
    }
    Ok(out)
}

/// The resolved shape of a `[patch]` replacement (same tuple as a normal
/// resolved dep).
type PatchDep = (
    Option<PathBuf>,
    String,
    bool,
    bool,
    Vec<String>,
    Option<String>,
    Option<LockedSource>,
);

/// Applies a `[patch]` entry to a registry dependency: path patches
/// become path deps, git patches become git edges, registry (version)
/// patches are unsupported.
#[allow(clippy::too_many_arguments)]
fn apply_patch(
    name: &str,
    patch: &DepValue,
    parent: &Package,
    _member: &Path,
    workspace_root: &Path,
    _inherited: &BTreeMap<String, DepValue>,
    sources: &dyn LockedSourceProvider,
) -> Result<PatchDep, CargoImportError> {
    match patch {
        DepValue::Table {
            path: Some(path),
            package,
            optional,
            default_features,
            features,
            ..
        } => Ok((
            // Path patches resolve relative to the workspace root.
            Some(workspace_root.join(path)),
            package.clone().unwrap_or_else(|| name.to_owned()),
            optional.unwrap_or(false),
            default_features.unwrap_or(true),
            features.clone().unwrap_or_default(),
            None,
            None,
        )),
        DepValue::Table {
            git: Some(git),
            rev,
            tag,
            branch,
            package,
            optional,
            default_features,
            features,
            ..
        } => {
            let selector = crate::model::GitSelector {
                url: crate::model::GitSelector::canonical_url(git),
                rev: rev.clone(),
                tag: tag.clone(),
                branch: branch.clone(),
            };
            let edge = RegistryEdge {
                parent: parent.id.clone(),
                extern_name: name.replace('-', "_"),
                package: package.clone().unwrap_or_else(|| name.to_owned()),
                req: "*".to_owned(),
                git: Some(selector),
                optional: optional.unwrap_or(false),
                default_features: default_features.unwrap_or(true),
                features: features.clone().unwrap_or_default(),
            };
            let locked = sources.locked_package(&edge)?;
            Ok((
                None,
                package.clone().unwrap_or_else(|| name.to_owned()),
                optional.unwrap_or(false),
                default_features.unwrap_or(true),
                features.clone().unwrap_or_default(),
                None,
                locked,
            ))
        }
        DepValue::Table {
            path: None,
            git: None,
            ..
        } => Err(CargoImportError::Unsupported(format!(
            "[patch] entry for {name:?} is a registry patch; only path and git patches are supported"
        ))),
        DepValue::Version(_) => Err(CargoImportError::Unsupported(format!(
            "[patch] entry for {name:?} is a registry patch; only path and git patches are supported"
        ))),
    }
}

/// The manifest's dependency tables after target-specific merging, plus
/// the set of dependency names that are optional in at least one target
/// table (cargo feature references validate against the union).
type MergedTables = (
    Vec<(String, DepValue)>,
    Vec<(String, DepValue)>,
    Vec<(String, DepValue)>,
    BTreeSet<String>,
);

/// Collects the manifest's general and target-specific dependency tables.
/// Every edge is retained because Cargo resolves and locks the union across
/// all platforms; the configured unit graph filters targets later.
fn merge_target_tables(
    manifest: &CargoManifest,
    _host_triple: &str,
) -> Result<MergedTables, CargoImportError> {
    let mut dependencies: Vec<(String, DepValue)> = manifest
        .dependencies
        .iter()
        .map(|(name, dep)| (name.clone(), dep.clone()))
        .collect();
    let mut build_dependencies: Vec<(String, DepValue)> = manifest
        .build_dependencies
        .iter()
        .map(|(name, dep)| (name.clone(), dep.clone()))
        .collect();
    let mut dev_dependencies: Vec<(String, DepValue)> = manifest
        .dev_dependencies
        .iter()
        .map(|(name, dep)| (name.clone(), dep.clone()))
        .collect();
    // Names optional in any table (union semantics, computed BEFORE the
    // host-specific merge: wgpu's `wgpu-hal` is optional in the wasm
    // target table but plain elsewhere, and cargo feature references
    // validate against the union).
    let mut optional_anywhere: BTreeSet<String> = BTreeSet::new();
    let is_optional = |dep: &DepValue| {
        matches!(
            dep,
            DepValue::Table {
                optional: Some(true),
                ..
            }
        )
    };
    for (name, dep) in manifest
        .dependencies
        .iter()
        .chain(&manifest.build_dependencies)
    {
        if is_optional(dep) {
            optional_anywhere.insert(name.clone());
        }
    }
    for (key, table) in &manifest.target {
        let _ = key;
        for (name, dep) in table.dependencies.iter().chain(&table.build_dependencies) {
            if is_optional(dep) {
                optional_anywhere.insert(name.clone());
            }
        }
        // Validate every cfg now, but defer evaluation until configured
        // units are built. This keeps the resolved graph host-independent.
        if key.trim_start().starts_with("cfg(") {
            let _ = tong_core::platform::eval_cfg(key, "x86_64-unknown-linux-gnu")
                .map_err(|error| CargoImportError::Unsupported(error.to_string()))?;
        }
        let with_target = |dep: &DepValue| -> DepValue {
            let mut dep = dep.clone();
            if let DepValue::Table { target, .. } = &mut dep {
                *target = Some(key.clone());
            }
            dep
        };
        for (name, dep) in &table.dependencies {
            dependencies.push((name.clone(), with_target(dep)));
        }
        for (name, dep) in &table.build_dependencies {
            build_dependencies.push((name.clone(), with_target(dep)));
        }
        for (name, dep) in &table.dev_dependencies {
            dev_dependencies.push((name.clone(), with_target(dep)));
        }
    }
    Ok((
        dependencies,
        build_dependencies,
        dev_dependencies,
        optional_anywhere,
    ))
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

/// Resolves the profile table: named profiles plus the per-package
/// `[profile.<name>.package.<spec>]` overrides (profile name, spec) →
/// fully merged profile.
type ProfileTables = BTreeMap<String, CargoProfile>;
type PackageOverrides = BTreeMap<(String, String), ProfileSpec>;

fn resolve_profiles(
    tables: &ProfileTables,
) -> Result<(BTreeMap<String, ProfileSpec>, PackageOverrides), CargoImportError> {
    // Resolve `inherits` chains first (Cargo: every profile implicitly
    // inherits `dev` unless `inherits` names another).
    let mut bases: BTreeMap<String, String> = BTreeMap::new();
    for name in tables.keys() {
        // The built-in `release` profile keeps its own defaults unless
        // `inherits` says otherwise; every other profile implicitly
        // inherits `dev` (Cargo semantics).
        let base = tables[name].inherits.clone().unwrap_or_else(|| {
            if name == "release" {
                "release".to_owned()
            } else {
                "dev".to_owned()
            }
        });
        bases.insert(name.clone(), base);
    }
    let mut resolved: BTreeMap<String, ProfileSpec> = BTreeMap::new();
    let mut package_overrides: BTreeMap<(String, String), ProfileSpec> = BTreeMap::new();
    for name in tables.keys() {
        let spec =
            resolve_profile_chain(name, tables, &bases, &mut resolved, &mut package_overrides)?;
        resolved.insert(name.clone(), spec);
    }
    Ok((resolved, package_overrides))
}

fn resolve_profile_chain(
    name: &str,
    tables: &BTreeMap<String, CargoProfile>,
    bases: &BTreeMap<String, String>,
    resolved: &mut BTreeMap<String, ProfileSpec>,
    package_overrides: &mut BTreeMap<(String, String), ProfileSpec>,
) -> Result<ProfileSpec, CargoImportError> {
    if let Some(spec) = resolved.get(name) {
        return Ok(spec.clone());
    }
    if name != "dev" && name != "release" && !tables.contains_key(name) {
        return Err(CargoImportError::Unsupported(format!(
            "profile {name:?} inherits unknown profile {name:?}"
        )));
    }
    let base_name = bases.get(name).cloned().unwrap_or_else(|| "dev".to_owned());
    if base_name == name && name != "dev" && name != "release" {
        return Err(CargoImportError::Unsupported(format!(
            "profile {name:?} inherits itself"
        )));
    }
    // dev/release are implicit roots; other names must be declared.
    let base_spec = if base_name == "dev" {
        ProfileSpec::dev()
    } else if base_name == "release" {
        ProfileSpec::release()
    } else if tables.contains_key(&base_name) {
        resolve_profile_chain(&base_name, tables, bases, resolved, package_overrides)?
    } else {
        return Err(CargoImportError::Unsupported(format!(
            "profile {name:?} inherits unknown profile {base_name:?}"
        )));
    };
    let mut spec = base_spec.clone();
    let table = &tables[name];
    if table.build_override.is_some() {
        return Err(CargoImportError::Unsupported(format!(
            "profile {name:?} build-override is not supported"
        )));
    }
    if let Some(package) = &table.package {
        for (spec, override_table) in package {
            // Cargo: package overrides may set opt-level, debug,
            // split-debuginfo, debug-assertions, overflow-checks, lto,
            // panic, rpath, strip, codegen-units, incremental; they merge
            // over the resolved base profile.
            if override_table.inherits.is_some() {
                return Err(CargoImportError::Unsupported(format!(
                    "profile {name:?} package override for {spec:?} sets inherits"
                )));
            }
            if override_table.codegen_backend.is_some() {
                return Err(CargoImportError::Unsupported(format!(
                    "profile {name:?} package override for {spec:?} sets codegen-backend"
                )));
            }
            if override_table.build_override.is_some() {
                return Err(CargoImportError::Unsupported(format!(
                    "profile {name:?} package override for {spec:?} sets build-override"
                )));
            }
            let mut merged = base_spec.clone();
            if let Some(level) = &override_table.opt_level {
                merged.opt_level = match level {
                    OptLevelValue::Num(n) => {
                        if *n > 3 {
                            return Err(CargoImportError::Unsupported(format!(
                                "profile {name:?} package override for {spec:?} \
                                 opt-level = {n}; expected 0-3, \"s\", or \"z\""
                            )));
                        }
                        n.to_string()
                    }
                    OptLevelValue::Str(s) => match s.as_str() {
                        "0" | "1" | "2" | "3" | "s" | "z" => s.clone(),
                        other => {
                            return Err(CargoImportError::Unsupported(format!(
                                "profile {name:?} package override for {spec:?} \
                                 opt-level = {other:?}; expected 0-3, \"s\", or \"z\""
                            )));
                        }
                    },
                };
            }
            if let Some(debug) = &override_table.debug {
                merged.debug = match debug {
                    DebugValue::Bool(b) => *b,
                    DebugValue::Num(n) => {
                        if *n > 2 {
                            return Err(CargoImportError::Unsupported(format!(
                                "profile {name:?} package override for {spec:?} \
                                 debug = {n}; expected 0, 1, 2, \"full\", or \"none\""
                            )));
                        }
                        *n > 0
                    }
                    DebugValue::Str(s) => match s.as_str() {
                        "full" | "true" => true,
                        "none" | "false" => false,
                        "line-tables-only" => true,
                        other => {
                            return Err(CargoImportError::Unsupported(format!(
                                "profile {name:?} package override for {spec:?} \
                                 debug = {other:?}; expected 0, 1, 2, \"full\", or \"none\""
                            )));
                        }
                    },
                };
            }
            if let Some(lto) = &override_table.lto {
                merged.lto = match lto {
                    LtoValue::Bool(true) => Lto::Fat,
                    LtoValue::Bool(false) => Lto::Off,
                    LtoValue::Str(s) => match s.as_str() {
                        "thin" => Lto::Thin,
                        "fat" | "true" => Lto::Fat,
                        "off" | "false" => Lto::Off,
                        other => {
                            return Err(CargoImportError::Unsupported(format!(
                                "profile {name:?} package override for {spec:?} \
                                 lto = {other:?}; expected \"thin\", \"fat\", \"off\""
                            )));
                        }
                    },
                };
            }
            if let Some(panic) = &override_table.panic {
                merged.panic = match panic.as_str() {
                    "unwind" => PanicStrategy::Unwind,
                    "abort" => PanicStrategy::Abort,
                    other => {
                        return Err(CargoImportError::Unsupported(format!(
                            "profile {name:?} package override for {spec:?} has invalid \
                             panic {other:?}"
                        )));
                    }
                };
            }
            if let Some(units) = override_table.codegen_units {
                merged.codegen_units = Some(units);
            }
            if let Some(value) = override_table.overflow_checks {
                merged.overflow_checks = Some(value);
            }
            if let Some(value) = override_table.debug_assertions {
                merged.debug_assertions = Some(value);
            }
            if let Some(value) = &override_table.strip {
                merged.strip = Some(value.clone());
            }
            if let Some(value) = override_table.rpath {
                merged.rpath = Some(value);
            }
            if let Some(value) = &override_table.split_debuginfo {
                merged.split_debuginfo = Some(value.clone());
            }
            if override_table.incremental == Some(true) {
                eprintln!(
                    "tong: warning: profile {name:?} package override for {spec:?} \
                     requests incremental compilation, which Tong keeps disabled"
                );
            }
            package_overrides.insert((name.to_owned(), spec.clone()), merged);
        }
    }
    if table.codegen_backend.is_some() {
        return Err(CargoImportError::Unsupported(format!(
            "profile {name:?} codegen-backend is not supported"
        )));
    }
    if table.incremental == Some(true) {
        // Incremental compilation stays disabled: Tong actions are
        // cacheable, and incremental artifacts are not (one structured
        // compatibility divergence; Cargo's builds would be incremental).
        eprintln!(
            "tong: warning: profile {name:?} requests incremental compilation, \
             which Tong keeps disabled for cacheability"
        );
    }
    if let Some(level) = &table.opt_level {
        spec.opt_level = match level {
            OptLevelValue::Num(n) => {
                if *n > 3 {
                    return Err(CargoImportError::Unsupported(format!(
                        "profile {name:?} opt-level = {n}; expected 0-3, \"s\", or \"z\""
                    )));
                }
                n.to_string()
            }
            OptLevelValue::Str(s) => match s.as_str() {
                "0" | "1" | "2" | "3" | "s" | "z" => s.clone(),
                other => {
                    return Err(CargoImportError::Unsupported(format!(
                        "profile {name:?} opt-level = {other:?}; expected 0-3, \"s\", or \"z\""
                    )));
                }
            },
        };
    }
    if let Some(debug) = &table.debug {
        spec.debug = match debug {
            DebugValue::Bool(b) => *b,
            DebugValue::Num(n) => {
                if *n > 2 {
                    return Err(CargoImportError::Unsupported(format!(
                        "profile {name:?} debug = {n}; expected 0, 1, 2, \"full\", \
                         \"line-tables-only\", or \"none\""
                    )));
                }
                *n > 0
            }
            DebugValue::Str(s) => match s.as_str() {
                "full" | "true" => true,
                "none" | "false" => false,
                "line-tables-only" => true,
                other => {
                    return Err(CargoImportError::Unsupported(format!(
                        "profile {name:?} debug = {other:?}; expected 0, 1, 2, \
                         \"full\", \"line-tables-only\", or \"none\""
                    )));
                }
            },
        };
    }
    if let Some(lto) = &table.lto {
        spec.lto = match lto {
            LtoValue::Bool(true) => Lto::Fat,
            LtoValue::Bool(false) => Lto::Off,
            LtoValue::Str(s) => match s.as_str() {
                "thin" => Lto::Thin,
                "fat" | "true" => Lto::Fat,
                "off" | "false" => Lto::Off,
                other => {
                    return Err(CargoImportError::Unsupported(format!(
                        "profile {name:?} lto = {other:?}; expected \"thin\", \
                         \"fat\", \"off\", true, or false"
                    )));
                }
            },
        };
    }
    if let Some(panic) = &table.panic {
        spec.panic = match panic.as_str() {
            "unwind" => PanicStrategy::Unwind,
            "abort" => PanicStrategy::Abort,
            other => {
                return Err(CargoImportError::Unsupported(format!(
                    "profile {name:?} panic = {other:?}; expected \"unwind\" or \"abort\""
                )));
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
    if let Some(split) = &table.split_debuginfo {
        match split.as_str() {
            "none" | "unpacked" | "packed" => spec.split_debuginfo = Some(split.clone()),
            other => {
                return Err(CargoImportError::Unsupported(format!(
                    "profile {name:?} split-debuginfo = {other:?}; expected \
                     \"none\", \"unpacked\", or \"packed\""
                )));
            }
        }
    }
    Ok(spec)
}

/// Loads the root-local cargo config: `.cargo/config.toml` preferred,
/// `.cargo/config` as fallback. User-home and ancestor configuration is
/// never read — it is not a declared workspace input. Parse errors are
/// hard errors.
fn load_config(workspace_root: &Path) -> Result<CargoConfig, CargoImportError> {
    let dir = workspace_root.join(".cargo");
    let toml_path = dir.join("config.toml");
    let legacy_path = dir.join("config");
    let path = if toml_path.is_file() {
        toml_path
    } else {
        legacy_path
    };
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Ok(CargoConfig::default());
        }
        Err(err) => {
            return Err(CargoImportError::Io(path.display().to_string(), err));
        }
    };
    toml::from_str(&text).map_err(|err| CargoImportError::Parse(path.display().to_string(), err))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Provider for provider-free tests: registry deps are rejected with
    /// the targeted "requires Tong.lock" diagnostic.
    struct NoLock;

    impl LockedSourceProvider for NoLock {
        fn locked_package(
            &self,
            edge: &RegistryEdge,
        ) -> Result<Option<LockedSource>, CargoImportError> {
            Err(CargoImportError::Unsupported(format!(
                "registry dependency `{}` requires Tong.lock; run `tong lock`",
                edge.package
            )))
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
resolver = "2"
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
        let model = import_cargo_workspace(
            &dir.path().join("ws"),
            "aarch64-apple-darwin",
            &NO_LOCK,
            None,
        )
        .unwrap();
        assert_eq!(model.packages.len(), 2);
        let cli = model
            .packages
            .iter()
            .find(|p| p.name == "calc-cli")
            .unwrap();
        assert_eq!(cli.deps.len(), 1);
        assert_eq!(cli.deps[0].extern_name, "calc_core");
        assert_eq!(cli.deps[0].package.name, "calc-core");
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
        let err = import_cargo_workspace(
            &dir.path().join("ws"),
            "aarch64-apple-darwin",
            &NO_LOCK,
            None,
        )
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
        let model = import_cargo_workspace(
            &dir.path().join("ws"),
            "aarch64-apple-darwin",
            &NO_LOCK,
            None,
        )
        .unwrap();
        assert_eq!(model.global_rustflags, vec!["--cfg", "advanced_mode"]);
        assert_eq!(model.global_env.get("APP_GREETING").unwrap(), "hello");
    }

    #[test]
    fn imports_external_path_dependency() {
        // A path dep outside the workspace is imported recursively, like
        // Cargo does (used by examples/04-voxel-city → sdl3-sys).
        let dir = write_tree(&[
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\"crates/app\"]\nresolver = \"2\"\n",
            ),
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
        let model = import_cargo_workspace(
            &dir.path().join("ws"),
            "aarch64-apple-darwin",
            &NO_LOCK,
            None,
        )
        .unwrap();
        assert_eq!(model.packages.len(), 2);
        let app = model.packages.iter().find(|p| p.name == "app").unwrap();
        assert_eq!(app.deps.len(), 1);
        assert_eq!(app.deps[0].extern_name, "sdl3_sys");
        assert_eq!(app.deps[0].package.name, "sdl3-sys");
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
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\"a\"]\nresolver = \"2\"\n",
            ),
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
        let err = import_cargo_workspace(
            &dir.path().join("ws"),
            "aarch64-apple-darwin",
            &NO_LOCK,
            None,
        )
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
resolver = "2"

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
        let model = import_cargo_workspace(
            &dir.path().join("ws"),
            "aarch64-apple-darwin",
            &NO_LOCK,
            None,
        )
        .unwrap();
        let app = model.packages.iter().find(|p| p.name == "app").unwrap();
        assert_eq!(app.version, "1.2.3");
        assert_eq!(app.edition, Edition::E2021);
    }

    #[test]
    fn applies_patch_to_inherited_registry_dependency() {
        let dir = write_tree(&[
            (
                "Cargo.toml",
                r#"
[workspace]
members = ["app", "shared"]
resolver = "2"

[workspace.dependencies]
shared = "1"

[patch.crates-io]
shared = { path = "shared" }
"#,
            ),
            (
                "app/Cargo.toml",
                r#"
[package]
name = "app"
version = "0.1.0"
edition = "2021"

[dependencies]
shared.workspace = true
"#,
            ),
            ("app/src/lib.rs", ""),
            (
                "shared/Cargo.toml",
                "[package]\nname = \"shared\"\nversion = \"1.0.0\"\nedition = \"2021\"\n",
            ),
            ("shared/src/lib.rs", ""),
        ]);
        let model = import_cargo_workspace(
            &dir.path().join("ws"),
            "aarch64-apple-darwin",
            &NO_LOCK,
            None,
        )
        .unwrap();
        let app = model.packages.iter().find(|pkg| pkg.name == "app").unwrap();
        assert_eq!(app.deps.len(), 1);
        assert_eq!(app.deps[0].package.name, "shared");
        assert!(matches!(app.deps[0].package.source, SourceId::Workspace(_)));
    }

    #[test]
    fn missing_workspace_package_inheritance_is_a_targeted_error() {
        let dir = write_tree(&[
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\"app\"]\nresolver = \"2\"\n",
            ),
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
        let err = import_cargo_workspace(
            &dir.path().join("ws"),
            "aarch64-apple-darwin",
            &NO_LOCK,
            None,
        )
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
resolver = "2"
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
        let model = import_cargo_workspace(
            &dir.path().join("ws"),
            "aarch64-apple-darwin",
            &NO_LOCK,
            None,
        )
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
resolver = "2"
"#,
            ),
            (
                "app/Cargo.toml",
                "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            ),
            ("app/src/lib.rs", ""),
            ("app/build.rs", "fn main() {}"),
        ]);
        let model = import_cargo_workspace(
            &dir.path().join("ws"),
            "aarch64-apple-darwin",
            &NO_LOCK,
            None,
        )
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
resolver = "2"
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
        let model = import_cargo_workspace(
            &dir.path().join("ws"),
            "aarch64-apple-darwin",
            &NO_LOCK,
            None,
        )
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
resolver = "2"
"#,
            ),
            (
                "app/Cargo.toml",
                "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\nbuild = true\n",
            ),
            ("app/src/lib.rs", ""),
        ]);
        let err = import_cargo_workspace(
            &dir.path().join("ws"),
            "aarch64-apple-darwin",
            &NO_LOCK,
            None,
        )
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
        let model = import_cargo_workspace(
            &dir.path().join("ws"),
            "aarch64-apple-darwin",
            &NO_LOCK,
            None,
        )
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
        let model = import_cargo_workspace(
            &dir.path().join("ws"),
            "aarch64-apple-darwin",
            &NO_LOCK,
            None,
        )
        .unwrap();
        // Target-specific deps stay in the model with their target
        // recorded (cargo locks all targets); the backend filters them at
        // plan time.
        let app = model.packages.iter().find(|p| p.name == "app").unwrap();
        let names: Vec<&str> = app.deps.iter().map(|d| d.package.name.as_str()).collect();
        assert!(names.contains(&"common"));
        assert!(names.contains(&"unix-only"));
        assert!(names.contains(&"win-only"));
        let win = app
            .deps
            .iter()
            .find(|d| d.package.name == "win-only")
            .unwrap();
        assert_eq!(win.target.as_deref(), Some("cfg(windows)"));
        assert_eq!(
            app.deps
                .iter()
                .find(|d| d.package.name == "unix-only")
                .unwrap()
                .target
                .as_deref(),
            Some("cfg(unix)")
        );

        // The Windows import keeps the same model shape (targets recorded).
        let model = import_cargo_workspace(
            &dir.path().join("ws"),
            "x86_64-pc-windows-msvc",
            &NO_LOCK,
            None,
        )
        .unwrap();
        let app = model.packages.iter().find(|p| p.name == "app").unwrap();
        let names: Vec<&str> = app.deps.iter().map(|d| d.package.name.as_str()).collect();
        assert!(names.contains(&"unix-only"));
        assert!(names.contains(&"win-only"));
    }

    #[test]
    fn preserves_same_name_edges_for_all_targets() {
        let dir = write_tree(&[
            (
                "Cargo.toml",
                r#"
[package]
name = "app"
version = "0.1.0"
edition = "2021"

[dependencies]
shared = { path = "../common" }

[target.'cfg(windows)'.dependencies]
shared = { path = "../windows" }
"#,
            ),
            ("src/main.rs", "fn main() {}"),
            (
                "../common/Cargo.toml",
                "[package]\nname = \"shared\"\nversion = \"1.0.0\"\nedition = \"2021\"\n",
            ),
            ("../common/src/lib.rs", ""),
            (
                "../windows/Cargo.toml",
                "[package]\nname = \"shared\"\nversion = \"1.0.0\"\nedition = \"2021\"\n",
            ),
            ("../windows/src/lib.rs", ""),
        ]);
        let model = import_cargo_workspace(
            &dir.path().join("ws"),
            "aarch64-apple-darwin",
            &NO_LOCK,
            None,
        )
        .unwrap();
        let app = model.packages.iter().find(|pkg| pkg.name == "app").unwrap();
        assert_eq!(app.deps.len(), 2);
        assert_eq!(
            app.deps
                .iter()
                .filter(|dep| dep.extern_name == "shared")
                .count(),
            2
        );
        assert!(app.deps.iter().any(|dep| dep.target.is_none()));
        assert!(
            app.deps
                .iter()
                .any(|dep| dep.target.as_deref() == Some("cfg(windows)"))
        );
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
        let model = import_cargo_workspace(
            &dir.path().join("ws"),
            "aarch64-apple-darwin",
            &NO_LOCK,
            None,
        )
        .unwrap();
        // Both target-keyed deps are in the model with their targets; the
        // backend filters at plan time.
        let app = model.packages.iter().find(|p| p.name == "app").unwrap();
        assert_eq!(app.deps.len(), 2);
        assert_eq!(app.deps[0].package.name, "unix-only");
        assert_eq!(app.deps[1].package.name, "win-only");
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
        let model = import_cargo_workspace(
            &dir.path().join("ws"),
            "aarch64-apple-darwin",
            &NO_LOCK,
            None,
        )
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
        let err = import_cargo_workspace(
            &dir.path().join("ws"),
            "aarch64-apple-darwin",
            &NO_LOCK,
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("strip"), "{err}");
    }

    #[test]
    fn selects_resolver_from_workspace_and_package() {
        // Explicit workspace resolver.
        let dir = write_tree(&[
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\"app\"]\nresolver = \"3\"\n",
            ),
            (
                "app/Cargo.toml",
                "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            ),
            ("app/src/lib.rs", ""),
        ]);
        let model = import_cargo_workspace(
            &dir.path().join("ws"),
            "aarch64-apple-darwin",
            &NO_LOCK,
            None,
        )
        .unwrap();
        assert_eq!(model.resolver, ResolverVersion::V3);

        // Resolver 1 is deliberately unsupported, even when explicit.
        let dir = write_tree(&[
            (
                "Cargo.toml",
                "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2024\"\nresolver = \"1\"\n",
            ),
            ("src/main.rs", "fn main() {}"),
        ]);
        let error = import_cargo_workspace(
            &dir.path().join("ws"),
            "aarch64-apple-darwin",
            &NO_LOCK,
            None,
        )
        .unwrap_err();
        assert!(error.to_string().contains("workspace.resolver = \"2\""));

        // Edition 2024 defaults to resolver 3; 2021 to 2.
        let dir = write_tree(&[
            (
                "Cargo.toml",
                "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
            ),
            ("src/main.rs", "fn main() {}"),
        ]);
        let model = import_cargo_workspace(
            &dir.path().join("ws"),
            "aarch64-apple-darwin",
            &NO_LOCK,
            None,
        )
        .unwrap();
        assert_eq!(model.resolver, ResolverVersion::V3);
        let dir = write_tree(&[
            (
                "Cargo.toml",
                "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            ),
            ("src/main.rs", "fn main() {}"),
        ]);
        let model = import_cargo_workspace(
            &dir.path().join("ws"),
            "aarch64-apple-darwin",
            &NO_LOCK,
            None,
        )
        .unwrap();
        assert_eq!(model.resolver, ResolverVersion::V2);

        // Unsupported resolver values are targeted errors.
        let dir = write_tree(&[
            (
                "Cargo.toml",
                "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\nresolver = \"4\"\n",
            ),
            ("src/main.rs", "fn main() {}"),
        ]);
        let err = import_cargo_workspace(
            &dir.path().join("ws"),
            "aarch64-apple-darwin",
            &NO_LOCK,
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("resolver"), "{err}");
    }
}
