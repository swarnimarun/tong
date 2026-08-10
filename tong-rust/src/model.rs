//! The Rust backend target model.
//!
//! Both `Tong.toml` targets and imported `Cargo.toml` manifests lower into
//! this model; the backend plans actions from it. The model is deliberately
//! close to Cargo's package model because Rust's compiler is package-based.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Component, Path, PathBuf};

use tong_core::canonical::{CanonicalEncode, Encoder};

/// Exact package identity: name + version + source.
///
/// This is the identity used everywhere a package is *keyed*: feature
/// resolution, the lockfile, planned-action maps, and dependency edges.
/// The declared name and version remain presentation/compiler fields
/// (`CARGO_PKG_*` env, `--crate-name`); they are never an identity.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PackageId {
    /// Declared package name (may contain hyphens).
    pub name: String,
    /// Resolved version.
    pub version: semver::Version,
    /// Canonical source.
    pub source: SourceId,
}

impl PackageId {
    /// The lockfile source string for this identity: `path+<rel>`,
    /// `registry+<index>`, or `git+<url>#<rev>`.
    pub fn lock_source(&self) -> String {
        self.source.lock_source()
    }
}

impl fmt::Display for PackageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.name, self.version)?;
        match &self.source {
            SourceId::Workspace(_) | SourceId::Path(_) => Ok(()),
            SourceId::Registry(url) => write!(f, " (registry {url})"),
            SourceId::Git { url, rev } => write!(f, " (git {url}#{rev})"),
        }
    }
}

impl CanonicalEncode for PackageId {
    fn encode(&self, enc: &mut Encoder) {
        enc.write_str(&self.name);
        enc.write_str(&self.version.to_string());
        match &self.source {
            SourceId::Workspace(rel) => {
                enc.write_u32(0);
                enc.write_str(rel);
            }
            SourceId::Path(rel) => {
                enc.write_u32(1);
                enc.write_str(rel);
            }
            SourceId::Registry(url) => {
                enc.write_u32(2);
                enc.write_str(url);
            }
            SourceId::Git { url, rev } => {
                enc.write_u32(3);
                enc.write_str(url);
                enc.write_str(rev);
            }
        }
    }
}

/// Canonical package source.
///
/// `Workspace` and `Path` carry normalized forward-slash lexical paths
/// relative to the workspace root (leading `../` for declared external
/// path dependencies) — never canonical absolute host paths, so action
/// digests never depend on where the workspace lives. Registry URLs and
/// Git URL/revision pairs are canonical source identities.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SourceId {
    /// A workspace member.
    Workspace(String),
    /// A path dependency (inside or outside the workspace root).
    Path(String),
    /// A registry source (canonical index URL).
    Registry(String),
    /// A Git source (canonical URL + exact commit).
    Git { url: String, rev: String },
}

impl SourceId {
    /// The lockfile source string: `path+<rel>`, `registry+<index>`, or
    /// `git+<url>#<rev>`.
    pub fn lock_source(&self) -> String {
        match self {
            Self::Workspace(rel) | Self::Path(rel) => format!("path+{rel}"),
            Self::Registry(url) => format!("registry+{url}"),
            Self::Git { url, rev } => format!("git+{url}#{rev}"),
        }
    }

    /// Parses a lockfile source string into a [`SourceId`].
    pub fn parse_lock_source(text: &str) -> Result<Self, String> {
        if let Some(url) = text.strip_prefix("registry+") {
            Ok(Self::Registry(url.to_owned()))
        } else if let Some(rest) = text.strip_prefix("git+") {
            let (url, rev) = rest
                .split_once('#')
                .ok_or_else(|| format!("git source {text:?} has no revision"))?;
            Ok(Self::Git {
                url: url.to_owned(),
                rev: rev.to_owned(),
            })
        } else {
            Err(format!("unsupported lock source {text:?}"))
        }
    }
}

/// Normalizes a lexical path: resolves `.`/`..` components without
/// touching the filesystem.
fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The normalized forward-slash path of `dir` relative to
/// `workspace_root` (lexical; leading `../` when the directory lies
/// outside the root).
pub fn source_rel_path(dir: &Path, workspace_root: &Path) -> String {
    let dir = normalize_lexical(dir);
    let root = normalize_lexical(workspace_root);
    let dir_components: Vec<Component<'_>> = dir.components().collect();
    let root_components: Vec<Component<'_>> = root.components().collect();
    let mut common = 0;
    while common < dir_components.len()
        && common < root_components.len()
        && dir_components[common] == root_components[common]
    {
        common += 1;
    }
    let mut out = PathBuf::new();
    for _ in common..root_components.len() {
        out.push("..");
    }
    for component in &dir_components[common..] {
        out.push(component.as_os_str());
    }
    out.to_string_lossy().replace('\\', "/")
}

/// Cargo resolver semantics selected for the workspace.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ResolverVersion {
    /// Resolver 1 (pre-2021 default): a single feature domain — features
    /// unify across normal, build, and dev dependencies.
    V1,
    /// Resolver 2 (2021+ default): dev-dependencies are a separate feature
    /// domain; normal and build dependencies unify.
    V2,
    /// Resolver 3 (2024+ default): build-dependencies are a separate host
    /// domain too — a package used as both a normal and a build dependency
    /// keeps independent feature sets.
    V3,
}

impl ResolverVersion {
    /// Cargo's resolver selection: an explicit `resolver = "1"|"2"|"3"`
    /// wins; otherwise the package edition picks the default (2021+ → 2,
    /// 2024 → 3, older → 1).
    pub fn from_manifest(resolver: Option<&str>, edition: Edition) -> Result<Self, String> {
        match resolver {
            Some("1") => Ok(Self::V1),
            Some("2") => Ok(Self::V2),
            Some("3") => Ok(Self::V3),
            Some(other) => Err(format!("unsupported resolver version {other:?}")),
            None => Ok(match edition {
                Edition::E2015 | Edition::E2018 => Self::V1,
                Edition::E2021 => Self::V2,
                Edition::E2024 => Self::V3,
            }),
        }
    }
}

/// The full set of Rust targets to build.
#[derive(Clone, Debug)]
pub struct RustModel {
    /// Workspace packages.
    pub packages: Vec<Package>,
    /// Identities of the workspace-member packages (feature-resolution
    /// seeds and lockfile roots).
    pub members: Vec<PackageId>,
    /// Named, resolved profiles.
    pub profiles: BTreeMap<String, ProfileSpec>,
    /// Imported prebuilt native libraries.
    pub cc_imports: Vec<CcImport>,
    /// Workspace-wide rustc flags (e.g. `.cargo/config.toml` `[build]`).
    pub global_rustflags: Vec<String>,
    /// Workspace-wide environment (e.g. `.cargo/config.toml` `[env]`).
    pub global_env: BTreeMap<String, String>,
    /// Resolved feature activation (set by the driver before planning).
    pub feature_map: crate::features::FeatureMap,
    /// Cargo resolver semantics for feature resolution.
    pub resolver: ResolverVersion,
    /// Workspace packages built by default (`[workspace] default_members`;
    /// empty = every member).
    pub default_members: Vec<PackageId>,
}

impl Default for RustModel {
    fn default() -> Self {
        Self {
            packages: Vec::new(),
            members: Vec::new(),
            profiles: BTreeMap::new(),
            cc_imports: Vec::new(),
            global_rustflags: Vec::new(),
            global_env: BTreeMap::new(),
            feature_map: crate::features::FeatureMap::default(),
            resolver: ResolverVersion::V2,
            default_members: Vec::new(),
        }
    }
}

/// A Rust package (one crate compilation unit).
#[derive(Clone, Debug)]
pub struct Package {
    /// Exact package identity (name + version + source).
    pub id: PackageId,
    /// Package name (may contain hyphens) — presentation/compiler field.
    pub name: String,
    /// Source root directory, relative to the workspace root.
    pub dir: PathBuf,
    /// Package version — presentation/compiler field.
    pub version: String,
    /// Rust edition.
    pub edition: Edition,
    /// Library target, if any (or proc macro).
    pub lib: Option<LibTarget>,
    /// Binary targets.
    pub bins: Vec<BinTarget>,
    /// Example targets (compiled like binaries; built in test/`--all-
    /// targets` builds).
    pub examples: Vec<ExampleTarget>,
    /// Test targets (`[[test]]`, `[[bench]]`, auto lib unit test).
    pub tests: Vec<TestTarget>,
    /// Build script, relative to `dir`.
    pub build_script: Option<PathBuf>,
    /// `links` value of the native library this package links (Cargo:
    /// one package per value; the build script's metadata is exported to
    /// direct dependents as `DEP_<LINKS>_<KEY>`).
    pub links: Option<String>,
    /// Normal dependencies.
    pub deps: Vec<Dep>,
    /// Build-script-only dependencies.
    pub build_deps: Vec<Dep>,
    /// Dev-dependencies (test/example builds only).
    pub dev_deps: Vec<Dep>,
    /// Declared features: feature name → list of `feature` / `dep:` /
    /// `dep/feat` / `dep?/feat` references.
    pub features: BTreeMap<String, Vec<String>>,
    /// Whether the package declares a `default` feature.
    pub has_default_feature: bool,
    /// Extra per-package rustc flags.
    pub rustflags: Vec<String>,
    /// Extra per-package environment.
    pub env: BTreeMap<String, String>,
}

/// Rust edition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edition {
    /// 2015
    E2015,
    /// 2018
    E2018,
    /// 2021
    E2021,
    /// 2024
    E2024,
}

impl Edition {
    /// The rustc `--edition` value.
    pub fn to_rustc(self) -> &'static str {
        match self {
            Self::E2015 => "2015",
            Self::E2018 => "2018",
            Self::E2021 => "2021",
            Self::E2024 => "2024",
        }
    }
}

/// The library target of a package.
#[derive(Clone, Debug)]
pub struct LibTarget {
    /// `[lib] name` override; the crate name used for the rlib filename
    /// and `--crate-name` (falls back to the package name).
    pub name: Option<String>,
    /// Crate types to compile; empty means `[Rlib]`.
    pub crate_types: Vec<CrateType>,
    /// Compile as a proc macro (host only).
    pub proc_macro: bool,
    /// Crate root, relative to `dir`.
    pub path: PathBuf,
}

/// A binary target.
#[derive(Clone, Debug)]
pub struct BinTarget {
    /// Output name.
    pub name: String,
    /// rustc `--crate-name` (defaults to the sanitized target name).
    pub crate_name: String,
    /// Crate root, relative to `dir`.
    pub path: PathBuf,
    /// Features that must all be active for this target to build.
    pub required_features: Vec<String>,
}

/// An example target (`[[example]]` or auto-discovered `examples/*`).
#[derive(Clone, Debug)]
pub struct ExampleTarget {
    /// Output name.
    pub name: String,
    /// rustc `--crate-name`.
    pub crate_name: String,
    /// Crate root, relative to `dir`.
    pub path: PathBuf,
    /// Features that must all be active for this target to build.
    pub required_features: Vec<String>,
}

/// A test target (`[[test]]`, `[[bench]]`, or the auto-derived lib unit
/// test).
#[derive(Clone, Debug)]
pub struct TestTarget {
    /// Target name (the test binary name).
    pub name: String,
    /// Crate root, relative to the package dir.
    pub path: PathBuf,
    /// Whether the target uses the libtest harness (`--test`).
    pub harness: bool,
    /// Whether the target is a doc test (run via rustdoc; the run action
    /// is planned in the action-parity wave).
    pub doc: bool,
    /// `true` makes the test-run action cacheable (native
    /// `cache-test-result = true`); Cargo-imported runs stay uncached.
    pub cache_test_result: bool,
    /// Features that must all be active for this target to build.
    pub required_features: Vec<String>,
}

/// A dependency edge on another package in the graph.
#[derive(Clone, Debug)]
pub struct Dep {
    /// `--extern` name used by the dependent crate.
    pub extern_name: String,
    /// Exact identity of the dependency package.
    pub package: PackageId,
    /// Optional dependency (only linked when activated via features).
    pub optional: bool,
    /// Whether the dependency's default feature is enabled.
    pub default_features: bool,
    /// Features requested on the dependency.
    pub features: Vec<String>,
    /// Target-specific dependency: the `cfg(...)` expression (e.g.
    /// `cfg(unix)`) or literal target triple that must match the host.
    pub target: Option<String>,
}

/// A git dependency selector (Cargo `git = "url"` plus at most one of
/// `rev`, `tag`, `branch`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitSelector {
    /// Repository URL (canonicalized: trailing `.git` stripped for
    /// HTTPS).
    pub url: String,
    /// Exact commit-ish (SHA or ref), resolved by `tong lock`; never
    /// re-resolved against a moving ref.
    pub rev: Option<String>,
    /// Tag, resolved to a commit only by `tong lock`.
    pub tag: Option<String>,
    /// Branch, resolved to a commit only by `tong lock`.
    pub branch: Option<String>,
}

impl GitSelector {
    /// Canonicalizes a repository URL for identity: HTTPS URLs lose a
    /// trailing `.git`; all other forms (file, ssh, scp-style) are
    /// unchanged — hosts, SSH syntax, and credentials are never rewritten.
    pub fn canonical_url(url: &str) -> String {
        if url.starts_with("https://") && url.ends_with(".git") {
            url[..url.len() - 4].to_owned()
        } else {
            url.to_owned()
        }
    }
}

/// An unresolved registry dependency edge, collected during import
/// (`Tong.lock` roots for the version resolver).
#[derive(Clone, Debug)]
pub struct RegistryEdge {
    /// The exact identity of the package declaring the dependency.
    pub parent: PackageId,
    /// `--extern` name used by the dependent crate.
    pub extern_name: String,
    /// Real package name.
    pub package: String,
    /// Version requirement, as written in the manifest (`*` for git deps
    /// without a version).
    pub req: String,
    /// Git source selector; `Some` makes this a git edge (resolved
    /// against the repository, never the index).
    pub git: Option<GitSelector>,
    /// Optional dependency (feature-activated).
    pub optional: bool,
    /// Whether the dependency's default feature is enabled.
    pub default_features: bool,
    /// Features requested on the dependency.
    pub features: Vec<String>,
}

/// Rust crate types the backend can compile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CrateType {
    /// rlib (default library type).
    Rlib,
    /// cdylib.
    Cdylib,
    /// staticlib.
    Staticlib,
    /// dylib.
    Dylib,
}

impl CrateType {
    /// The rustc `--crate-type` value.
    pub fn to_rustc(self) -> &'static str {
        match self {
            Self::Rlib => "rlib",
            Self::Cdylib => "cdylib",
            Self::Staticlib => "staticlib",
            Self::Dylib => "dylib",
        }
    }
}

/// An imported prebuilt native library (PLAN.md section 12, `cc_import`).
#[derive(Clone, Debug)]
pub struct CcImport {
    /// Target name used in the manifest.
    pub name: String,
    /// Path to the shared library on the host (system capture, non-portable).
    pub shared: PathBuf,
    /// `-l` link name (e.g. `SDL3.0`).
    pub link_name: String,
}

/// The crate name of a package's library: the `[lib] name` override when
/// present, else the sanitized package name. Binaries, tests, and
/// dependents must agree on this for `--crate-name` and the rlib filename.
pub fn lib_crate_name(pkg: &Package) -> String {
    match &pkg.lib {
        Some(lib) => lib
            .name
            .as_deref()
            .map(crate_name)
            .unwrap_or_else(|| crate_name(&pkg.name)),
        None => crate_name(&pkg.name),
    }
}

/// Sanitizes a package/target name for rustc (`--crate-name`).
pub fn crate_name(name: &str) -> String {
    name.replace('-', "_")
}

impl Package {
    /// A deterministic, label-independent rendering of the package's
    /// manifest contribution.
    ///
    /// Native `Tong.toml` targets capture this instead of the raw manifest
    /// file: renaming a `[target.<key>]` table (the graph label) never
    /// changes the canonical form as long as the stable fields
    /// (`package_name`, `version`, `crate_name`, `output_name`, deps,
    /// features, …) stay unchanged — label renames stay digest-neutral.
    /// Dependency edges are rendered as their resolved identities
    /// (extern name + package name/version), never as label strings.
    pub fn canonical_manifest(&self) -> String {
        // Paths render relative to the package dir: absolute host paths
        // never enter a digest (the backend resolves crate roots to
        // absolute paths at model-build time).
        let relative = |path: &Path| -> String {
            path.strip_prefix(&self.dir)
                .unwrap_or(path)
                .to_string_lossy()
                .into_owned()
        };
        let mut out = String::new();
        out.push_str(&format!("[package]\nname = {}\n", toml_quote(&self.name)));
        out.push_str(&format!("version = {}\n", toml_quote(&self.version)));
        out.push_str(&format!(
            "edition = {}\n",
            toml_quote(self.edition.to_rustc())
        ));
        if let Some(script) = &self.build_script {
            out.push_str(&format!(
                "build_script = {}\n",
                toml_quote(&relative(script))
            ));
        }
        out.push_str(&format!("rustflags = {:?}\n", self.rustflags));
        if !self.env.is_empty() {
            out.push_str("env = [\n");
            for (key, value) in &self.env {
                out.push_str(&format!("  {} = {}\n", toml_quote(key), toml_quote(value)));
            }
            out.push_str("]\n");
        }
        out.push_str(&format!(
            "features = [\n{}\n]\n",
            self.features
                .iter()
                .map(|(name, refs)| format!("  {} = {:?}", toml_quote(name), refs))
                .collect::<Vec<_>>()
                .join("\n")
        ));
        if let Some(lib) = &self.lib {
            out.push_str("lib = {\n");
            out.push_str(&format!(
                "  crate_name = {},\n",
                toml_quote(lib.name.as_deref().unwrap_or(""))
            ));
            out.push_str(&format!(
                "  crate_types = {:?},\n",
                lib.crate_types
                    .iter()
                    .map(|t| t.to_rustc())
                    .collect::<Vec<_>>()
            ));
            out.push_str(&format!("  proc_macro = {},\n", lib.proc_macro));
            out.push_str(&format!("  path = {},\n", toml_quote(&relative(&lib.path))));
            out.push_str("}\n");
        }
        for bin in &self.bins {
            out.push_str(&format!(
                "bin = {{ name = {}, crate_name = {}, path = {}, required_features = {:?} }}\n",
                toml_quote(&bin.name),
                toml_quote(&bin.crate_name),
                toml_quote(&relative(&bin.path)),
                bin.required_features
            ));
        }
        for test in &self.tests {
            out.push_str(&format!(
                "test = {{ name = {}, path = {}, harness = {}, doc = {}, cache_test_result = {}, required_features = {:?} }}\n",
                toml_quote(&test.name),
                toml_quote(&relative(&test.path)),
                test.harness,
                test.doc,
                test.cache_test_result,
                test.required_features
            ));
        }
        for dep in self
            .deps
            .iter()
            .chain(self.build_deps.iter())
            .chain(self.dev_deps.iter())
        {
            out.push_str(&format!(
                "dep = {{ extern = {}, package = {}, version = {}, optional = {}, default_features = {}, features = {:?} }}\n",
                toml_quote(&dep.extern_name),
                toml_quote(&dep.package.name),
                toml_quote(&dep.package.version.to_string()),
                dep.optional,
                dep.default_features,
                dep.features
            ));
        }
        out
    }
}

fn toml_quote(text: &str) -> String {
    format!("{text:?}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crate_names_sanitize_hyphens() {
        assert_eq!(crate_name("voxel-city"), "voxel_city");
        assert_eq!(crate_name("plain"), "plain");
    }

    fn pkg(name: &str) -> Package {
        Package {
            id: PackageId {
                name: name.to_owned(),
                version: semver::Version::new(0, 1, 0),
                source: SourceId::Workspace(".".to_owned()),
            },
            name: name.to_owned(),
            dir: PathBuf::from("."),
            version: "0.1.0".to_owned(),
            edition: Edition::E2021,
            lib: None,
            bins: Vec::new(),
            examples: Vec::new(),
            tests: Vec::new(),
            build_script: None,
            links: None,
            deps: Vec::new(),
            build_deps: Vec::new(),
            dev_deps: Vec::new(),
            features: BTreeMap::new(),
            has_default_feature: false,
            rustflags: Vec::new(),
            env: BTreeMap::new(),
        }
    }

    #[test]
    fn lib_crate_name_uses_lib_override() {
        let mut pkg = pkg("my-lib");
        assert_eq!(lib_crate_name(&pkg), "my_lib");
        pkg.lib = Some(LibTarget {
            name: Some("renamed".to_owned()),
            crate_types: Vec::new(),
            proc_macro: false,
            path: PathBuf::from("src/lib.rs"),
        });
        assert_eq!(lib_crate_name(&pkg), "renamed");
    }

    #[test]
    fn package_ids_distinguish_versions_and_sources() {
        let a1 = PackageId {
            name: "alpha".to_owned(),
            version: semver::Version::new(1, 0, 0),
            source: SourceId::Registry("https://index.crates.io".to_owned()),
        };
        let a2 = PackageId {
            name: "alpha".to_owned(),
            version: semver::Version::new(2, 0, 0),
            source: SourceId::Registry("https://index.crates.io".to_owned()),
        };
        assert_ne!(a1, a2);
        let member = PackageId {
            name: "alpha".to_owned(),
            version: semver::Version::new(0, 1, 0),
            source: SourceId::Workspace("crates/alpha".to_owned()),
        };
        assert_ne!(a1, member);
        // Hash agrees with equality.
        let mut set = std::collections::BTreeSet::new();
        set.insert(a1.clone());
        set.insert(a2.clone());
        set.insert(member.clone());
        assert_eq!(set.len(), 3);
        assert_eq!(a1.lock_source(), "registry+https://index.crates.io");
        assert_eq!(member.lock_source(), "path+crates/alpha");
    }

    #[test]
    fn source_rel_paths_are_lexical_forward_slash() {
        let root = Path::new("/ws");
        assert_eq!(
            source_rel_path(Path::new("/ws/crates/app"), root),
            "crates/app"
        );
        assert_eq!(
            source_rel_path(Path::new("/ws/crates/app/../core"), root),
            "crates/core"
        );
        assert_eq!(
            source_rel_path(Path::new("/ws/../shared/x"), root),
            "../shared/x"
        );
        assert_eq!(source_rel_path(Path::new("/other/y"), root), "../other/y");
    }

    #[test]
    fn resolver_defaults_follow_editions() {
        assert_eq!(
            ResolverVersion::from_manifest(None, Edition::E2015).unwrap(),
            ResolverVersion::V1
        );
        assert_eq!(
            ResolverVersion::from_manifest(None, Edition::E2018).unwrap(),
            ResolverVersion::V1
        );
        assert_eq!(
            ResolverVersion::from_manifest(None, Edition::E2021).unwrap(),
            ResolverVersion::V2
        );
        assert_eq!(
            ResolverVersion::from_manifest(None, Edition::E2024).unwrap(),
            ResolverVersion::V3
        );
        assert_eq!(
            ResolverVersion::from_manifest(Some("1"), Edition::E2024).unwrap(),
            ResolverVersion::V1
        );
        assert!(ResolverVersion::from_manifest(Some("4"), Edition::E2021).is_err());
    }
}
#[derive(Clone, Debug)]
pub struct ProfileSpec {
    /// Optimization level: `0`-`3`, `s`, `z`.
    pub opt_level: String,
    /// Debug info on.
    pub debug: bool,
    /// LTO mode.
    pub lto: Lto,
    /// Panic strategy.
    pub panic: PanicStrategy,
    /// Codegen units (None = rustc default).
    pub codegen_units: Option<u32>,
    /// Overflow checks (None = rustc default).
    pub overflow_checks: Option<bool>,
    /// Debug assertions (None = rustc default).
    pub debug_assertions: Option<bool>,
    /// Strip setting: `none`, `debuginfo`, or `symbols` (None = rustc
    /// default).
    pub strip: Option<String>,
    /// Whether rpath is passed to the linker (None = rustc default).
    pub rpath: Option<bool>,
    /// `-C split-debuginfo` value: `none`, `unpacked`, or `packed`
    /// (None = rustc default).
    pub split_debuginfo: Option<String>,
}

/// LTO mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lto {
    /// No LTO.
    Off,
    /// Thin LTO.
    Thin,
    /// Fat LTO.
    Fat,
}

/// Panic strategy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanicStrategy {
    /// Unwind.
    Unwind,
    /// Abort.
    Abort,
}

impl Default for ProfileSpec {
    fn default() -> Self {
        Self::dev()
    }
}

impl ProfileSpec {
    /// Cargo-compatible `dev` defaults.
    pub fn dev() -> Self {
        Self {
            opt_level: "0".to_owned(),
            debug: true,
            lto: Lto::Off,
            panic: PanicStrategy::Unwind,
            codegen_units: None,
            overflow_checks: Some(true),
            debug_assertions: Some(true),
            strip: None,
            rpath: None,
            split_debuginfo: None,
        }
    }

    /// Cargo-compatible `release` defaults.
    pub fn release() -> Self {
        Self {
            opt_level: "3".to_owned(),
            debug: false,
            lto: Lto::Off,
            panic: PanicStrategy::Unwind,
            codegen_units: None,
            overflow_checks: Some(false),
            debug_assertions: Some(false),
            strip: None,
            rpath: None,
            split_debuginfo: None,
        }
    }

    /// The rustc flags derived from this profile (PLAN.md section 8.5:
    /// incremental compilation stays off so actions remain cacheable).
    pub fn rustc_flags(&self) -> Vec<String> {
        let mut flags = Vec::new();
        flags.push("-C".to_owned());
        flags.push(format!("opt-level={}", self.opt_level));
        flags.push("-C".to_owned());
        flags.push(format!("debuginfo={}", if self.debug { 2 } else { 0 }));
        flags.push("-C".to_owned());
        flags.push(format!(
            "lto={}",
            match self.lto {
                Lto::Off => "off",
                Lto::Thin => "thin",
                Lto::Fat => "yes",
            }
        ));
        flags.push("-C".to_owned());
        flags.push(format!(
            "panic={}",
            match self.panic {
                PanicStrategy::Unwind => "unwind",
                PanicStrategy::Abort => "abort",
            }
        ));
        if let Some(checks) = self.overflow_checks {
            flags.push("-C".to_owned());
            flags.push(format!(
                "overflow-checks={}",
                if checks { "on" } else { "off" }
            ));
        }
        if let Some(assertions) = self.debug_assertions {
            flags.push("-C".to_owned());
            flags.push(format!(
                "debug-assertions={}",
                if assertions { "on" } else { "off" }
            ));
        }
        if let Some(strip) = &self.strip {
            flags.push("-C".to_owned());
            flags.push(format!("strip={strip}"));
        }
        if let Some(rpath) = self.rpath {
            flags.push("-C".to_owned());
            flags.push(format!("rpath={}", if rpath { "yes" } else { "no" }));
        }
        if let Some(units) = self.codegen_units {
            flags.push("-C".to_owned());
            flags.push(format!("codegen-units={units}"));
        }
        if let Some(split) = &self.split_debuginfo {
            flags.push("-C".to_owned());
            flags.push(format!("split-debuginfo={split}"));
        }
        flags
    }
}
