//! `Tong.toml` manifest loading.
//!
//! The canonical build description (PLAN.md section 6.1). Version 1
//! (`schema = 1`, required) carries the workspace (root package plus
//! member manifests with glob lists), the toolchain, named profiles, and
//! named targets. Target rules are interpreted by backends; the graph
//! layer only models the file. A missing or unsupported schema is an
//! error — never guessed.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::Path;

use serde::Deserialize;

/// The supported manifest schema version.
pub const MANIFEST_SCHEMA: u32 = 1;

/// A parsed `Tong.toml`.
#[derive(Clone, Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// Required schema version (`schema = 1`).
    pub schema: Option<u32>,
    /// Workspace metadata: members and default members (globs).
    #[serde(default)]
    pub workspace: Workspace,
    /// Toolchain configuration.
    #[serde(default)]
    pub toolchain: Toolchain,
    /// Named build profiles (`[profile.<name>]`).
    #[serde(default)]
    pub profile: BTreeMap<String, ProfileConfig>,
    /// Named targets (`[target.<name>]`).
    #[serde(default)]
    pub target: BTreeMap<String, TargetConfig>,
    /// Store policy (`[store]`): shared-store location, retention, budget.
    #[serde(default)]
    pub store: Option<StoreConfig>,
    /// Registry configuration (`[registry]`): the index URL.
    #[serde(default)]
    pub registry: Option<RegistryConfig>,
    /// Execution policy (`[policy]`): sandbox level, network access.
    #[serde(default)]
    pub policy: Option<PolicyConfig>,
}

/// Execution policy configuration (`[policy]`).
///
/// Sandboxing is opt-in (default `l1` — clean environment) until certified
/// per platform (PLAN.md section 11). Network access is denied by default;
/// `network = "allow"` lets run actions reach the network and makes them
/// uncacheable.
#[derive(Clone, Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct PolicyConfig {
    /// Sandbox level: `l1`, `l2`, `l3`, or `l4`.
    pub sandbox: Option<String>,
    /// Network access for run actions: `"deny"` (default) or `"allow"`.
    pub network: Option<String>,
}

/// Registry configuration (`[registry]`).
///
/// `index` selects the crate registry index (default crates.io's sparse
/// index); the environment variable `TONG_REGISTRY_INDEX` wins over the
/// manifest.
#[derive(Clone, Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct RegistryConfig {
    /// Index URL: `sparse+https://…`, `https://…`, or `file://…`.
    pub index: Option<String>,
}

impl Manifest {
    /// Loads and parses `Tong.toml` from a directory.
    pub fn load(dir: &Path) -> Result<Self, ManifestError> {
        let path = dir.join("Tong.toml");
        let text = fs::read_to_string(&path)
            .map_err(|err| ManifestError::Io(path.display().to_string(), err))?;
        let manifest: Manifest = toml::from_str(&text)
            .map_err(|err| ManifestError::Parse(path.display().to_string(), err))?;
        Self::validate(manifest).map_err(ManifestError::Schema)
    }

    /// Parses manifest text (pathless; used by tests).
    pub fn parse(text: &str) -> Result<Self, ManifestError> {
        let manifest: Manifest =
            toml::from_str(text).map_err(|err| ManifestError::Parse(String::new(), err))?;
        Self::validate(manifest).map_err(ManifestError::Schema)
    }

    /// Enforces the required schema version — never guessed.
    fn validate(manifest: Manifest) -> Result<Self, String> {
        if manifest.schema != Some(MANIFEST_SCHEMA) {
            return Err(match manifest.schema {
                Some(schema) => format!(
                    "Tong.toml schema {schema} is not supported (this Tong speaks \
                     schema = {MANIFEST_SCHEMA})"
                ),
                None => format!("Tong.toml requires schema = {MANIFEST_SCHEMA}"),
            });
        }
        Ok(manifest)
    }
}

/// Workspace metadata.
#[derive(Clone, Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Workspace {
    /// Workspace display name.
    pub name: Option<String>,
    /// Member directories (globs like `crates/*`); each member has its
    /// own `Tong.toml` with package-local targets.
    #[serde(default)]
    pub members: Vec<String>,
    /// Members built by default; absent = all members.
    #[serde(default)]
    pub default_members: Vec<String>,
}

/// Toolchain configuration.
#[derive(Clone, Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Toolchain {
    /// Rust toolchain entry.
    pub rust: RustToolchain,
}

/// Rust toolchain configuration.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RustToolchain {
    /// Toolchain kind. Version 1 supports `system` (system capture,
    /// non-portable; PLAN.md section 5) and `dist` (downloaded via
    /// `tong toolchain fetch rust --version <ver>`, portable).
    pub kind: String,
    /// Dist toolchain version (e.g. `1.90.0`); required for `dist`.
    pub version: Option<String>,
    /// Additional rustup target triples the workspace builds for. The
    /// host triple is always available; a missing listed target fails
    /// before planning with the `tong toolchain fetch rust` remedy.
    #[serde(default)]
    pub targets: Vec<String>,
}

impl Default for RustToolchain {
    fn default() -> Self {
        Self {
            kind: "system".to_owned(),
            version: None,
            targets: Vec::new(),
        }
    }
}

/// A named build profile (PLAN.md section 8.5).
#[derive(Clone, Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ProfileConfig {
    /// Optimization level: `0`-`3`, `s`, or `z`.
    #[serde(default)]
    pub opt_level: Option<OptLevel>,
    /// Debug info on/off.
    pub debug: Option<bool>,
    /// LTO: bool, `"thin"`, or `"fat"`.
    pub lto: Option<Lto>,
    /// Panic strategy: `unwind` or `abort`.
    pub panic: Option<String>,
    /// Codegen units.
    pub codegen_units: Option<u32>,
    /// Overflow checks.
    pub overflow_checks: Option<bool>,
    /// Debug assertions.
    pub debug_assertions: Option<bool>,
    /// Strip setting: `none`, `debuginfo`, or `symbols`.
    pub strip: Option<String>,
    /// Pass rpath to the linker.
    pub rpath: Option<bool>,
}

/// Optimization level value.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum OptLevel {
    /// Numeric level.
    Num(u8),
    /// `s` or `z`.
    Str(String),
}

impl OptLevel {
    /// The rustc flag value.
    pub fn to_rustc(&self) -> String {
        match self {
            Self::Num(level) => level.to_string(),
            Self::Str(value) => value.clone(),
        }
    }
}

/// LTO setting.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum Lto {
    /// On/off.
    Bool(bool),
    /// `thin` or `fat`.
    Str(String),
}

/// Store policy configuration (`[store]`).
///
/// `dir` relocates the content-addressed store (shared-store mode); it must
/// be a relative path without `..` components and resolves against the
/// workspace root. `retention` and `max_size` configure automatic GC
/// (human formats: `7d`, `10G`); environment overrides `TONG_STORE_DIR`,
/// `TONG_STORE_RETENTION`, and `TONG_STORE_MAX_SIZE` win over the manifest.
#[derive(Clone, Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct StoreConfig {
    /// Store directory, relative to the workspace root.
    pub dir: Option<String>,
    /// Age floor for deleting unmarked cache objects.
    pub retention: Option<String>,
    /// Store size budget; GC deletes unmarked objects below this.
    pub max_size: Option<String>,
}

/// A target dependency: a bare label string, or a table with an alias,
/// feature selection, and optionality.
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum TargetDepConfig {
    /// `deps = [":core"]` — non-optional, default features.
    Label(String),
    /// `deps = [{ label = "//crates/core:core", alias = "core",
    /// optional = true, default_features = false, features = ["serde"] }]`.
    Table {
        /// The target label (`:local`, `//member/path:name`,
        /// `//member/path`).
        label: String,
        /// `--extern` name (defaults to the target name).
        alias: Option<String>,
        /// Optional dependency (activated via features).
        optional: Option<bool>,
        /// Disable the dependency's default feature (Cargo's
        /// `default-features` is accepted as an alias).
        #[serde(alias = "default-features")]
        default_features: Option<bool>,
        /// Features requested on the dependency.
        features: Option<Vec<String>>,
    },
}

/// A named target.
#[derive(Clone, Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct TargetConfig {
    /// Backend rule name: `rust_binary`, `rust_library`, `rust_proc_macro`,
    /// `rust_test`, `rust_example`, `rust_bench`, `rust_doc_test`, or
    /// `cc_import`. Unknown rules are hard errors.
    pub rule: String,
    /// Stable package presentation name (defaults to the target key).
    /// Renaming the table key is digest-neutral when these stable fields
    /// stay unchanged.
    pub package_name: Option<String>,
    /// Package version (defaults to `0.0.0`).
    pub version: Option<String>,
    /// rustc `--crate-name` (defaults to the sanitized package name).
    pub crate_name: Option<String>,
    /// Assembled binary name (defaults to the target key).
    pub output_name: Option<String>,
    /// Crate root source file, resolved beneath `package_root`.
    pub crate_root: Option<String>,
    /// Rust edition: `2015`, `2018`, `2021`, or `2024`.
    pub edition: Option<String>,
    /// Package root directory (defaults to the containing manifest's
    /// directory); `crate_root`, `build_script`, and `cc_import.shared`
    /// resolve beneath it.
    pub package_root: Option<String>,
    /// Dependencies as labels or structured tables.
    #[serde(default)]
    pub deps: Vec<TargetDepConfig>,
    /// Dev-dependencies (used by test/example targets).
    #[serde(default)]
    pub dev_deps: Vec<TargetDepConfig>,
    /// Extra rustc flags.
    #[serde(default)]
    pub rustflags: Vec<String>,
    /// Per-target environment variables.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Declared features: feature name → references (`dep:x`, `x/feat`,
    /// `x?/feat`, plain names).
    #[serde(default)]
    pub features: BTreeMap<String, Vec<String>>,
    /// Build-script source, resolved beneath `package_root`.
    pub build_script: Option<String>,
    /// Crate types for a library target.
    #[serde(default)]
    pub crate_types: Vec<String>,
    /// Compile as a proc macro.
    pub proc_macro: Option<bool>,
    /// Features that must all be active for this target to build (Cargo's
    /// `required-features` is accepted as an alias).
    #[serde(alias = "required-features")]
    pub required_features: Option<Vec<String>>,
    /// Whether a test/bench target uses the libtest harness.
    pub harness: Option<bool>,
    /// `true` makes a test-run action cacheable (default `false`: test
    /// runs re-execute every time; Cargo's `cache-test-result` is accepted
    /// as an alias).
    #[serde(alias = "cache-test-result")]
    pub cache_test_result: Option<bool>,
    /// `cc_import`: path to the shared library to import (resolved
    /// beneath `package_root`).
    pub shared: Option<String>,
    /// `cc_import`: `-l` link name.
    pub link_name: Option<String>,
}

/// Manifest loading error.
#[derive(Debug)]
pub enum ManifestError {
    /// The file could not be read.
    Io(String, io::Error),
    /// The file was not valid TOML.
    Parse(String, toml::de::Error),
    /// The schema version is missing or unsupported.
    Schema(String),
}

impl std::fmt::Display for ManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(path, err) => write!(f, "cannot read {path}: {err}"),
            Self::Parse(path, err) => {
                if path.is_empty() {
                    write!(f, "cannot parse Tong.toml: {err}")
                } else {
                    write!(f, "cannot parse {path}: {err}")
                }
            }
            Self::Schema(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for ManifestError {}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"
schema = 1

[workspace]
name = "hello"

[toolchain.rust]
kind = "system"
targets = ["aarch64-apple-darwin"]

[profile.release]
opt_level = 3
lto = "thin"
panic = "abort"
codegen_units = 16
overflow_checks = false

[target.hello]
rule = "rust_binary"
package_name = "hello-app"
version = "1.2.3"
crate_name = "hello_app"
output_name = "hello-bin"
crate_root = "src/main.rs"
edition = "2021"
deps = [
  { label = ":greet", alias = "greet", features = ["serde"] },
]
rustflags = ["-D", "warnings"]
env = { GREETING = "hello" }

[target.greet]
rule = "rust_library"
"#;

    #[test]
    fn parses_valid_manifest() {
        let manifest = Manifest::parse(VALID).unwrap();
        assert_eq!(manifest.schema, Some(1));
        assert_eq!(manifest.workspace.name.as_deref(), Some("hello"));
        assert_eq!(manifest.toolchain.rust.kind, "system");
        assert_eq!(
            manifest.toolchain.rust.targets,
            vec!["aarch64-apple-darwin".to_owned()]
        );
        let release = manifest.profile.get("release").unwrap();
        assert_eq!(release.opt_level.as_ref().unwrap().to_rustc(), "3");
        assert_eq!(release.lto, Some(Lto::Str("thin".to_owned())));
        assert_eq!(release.panic.as_deref(), Some("abort"));
        let hello = manifest.target.get("hello").unwrap();
        assert_eq!(hello.rule, "rust_binary");
        assert_eq!(hello.package_name.as_deref(), Some("hello-app"));
        assert_eq!(hello.crate_name.as_deref(), Some("hello_app"));
        assert_eq!(hello.output_name.as_deref(), Some("hello-bin"));
        match &hello.deps[0] {
            TargetDepConfig::Table {
                label,
                alias,
                features,
                ..
            } => {
                assert_eq!(label, ":greet");
                assert_eq!(alias.as_deref(), Some("greet"));
                assert_eq!(
                    features.as_deref(),
                    Some(vec!["serde".to_owned()].as_slice())
                );
            }
            _ => panic!("structured dep"),
        }
        assert_eq!(hello.rustflags, vec!["-D", "warnings"]);
        assert_eq!(hello.env.get("GREETING").unwrap(), "hello");
    }

    #[test]
    fn requires_schema() {
        let err = Manifest::parse("[workspace]\nname = \"x\"\n").unwrap_err();
        assert!(err.to_string().contains("requires schema = 1"), "{err}");
        let err = Manifest::parse("schema = 2\n").unwrap_err();
        assert!(err.to_string().contains("schema 2"), "{err}");
    }

    #[test]
    fn rejects_unknown_fields() {
        let err =
            Manifest::parse("schema = 1\n[workspace]\nname = \"x\"\nunknown = 1\n").unwrap_err();
        assert!(err.to_string().contains("unknown"), "{err}");
    }

    #[test]
    fn parses_members_and_policy() {
        let manifest = Manifest::parse(
            r#"
schema = 1

[workspace]
name = "ws"
members = ["crates/*", "apps/cli"]

[policy]
sandbox = "l2"
network = "allow"
"#,
        )
        .unwrap();
        assert_eq!(manifest.workspace.members, vec!["crates/*", "apps/cli"]);
        assert_eq!(
            manifest.policy.as_ref().unwrap().network.as_deref(),
            Some("allow")
        );
        assert_eq!(
            manifest.policy.as_ref().unwrap().sandbox.as_deref(),
            Some("l2")
        );
    }
}
