//! `Tong.toml` manifest loading.
//!
//! The canonical build description (PLAN.md section 6.1). Version 1 carries
//! the workspace, the (system-captured) toolchain, named profiles, and
//! named targets. Target rules are interpreted by backends; the graph layer
//! only models the file.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::Path;

use serde::Deserialize;

/// A parsed `Tong.toml`.
#[derive(Clone, Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// Workspace metadata.
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
    /// Execution policy (`[policy]`): sandbox level.
    #[serde(default)]
    pub policy: Option<PolicyConfig>,
}

/// Execution policy configuration (`[policy]`).
///
/// Sandboxing is opt-in (default `l1` — clean environment) until certified
/// per platform (PLAN.md section 11).
#[derive(Clone, Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct PolicyConfig {
    /// Sandbox level: `l1`, `l2`, `l3`, or `l4`.
    pub sandbox: Option<String>,
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
        Self::parse(&text).map_err(|err| ManifestError::Parse(path.display().to_string(), err))
    }

    /// Parses manifest text.
    pub fn parse(text: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(text)
    }
}

/// Workspace metadata.
#[derive(Clone, Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Workspace {
    /// Workspace display name.
    pub name: Option<String>,
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
    /// Toolchain kind. Version 1 supports `system` only (system capture,
    /// non-portable; PLAN.md section 5).
    pub kind: String,
}

impl Default for RustToolchain {
    fn default() -> Self {
        Self {
            kind: "system".to_owned(),
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

/// A named target.
#[derive(Clone, Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct TargetConfig {
    /// Backend rule name, e.g. `rust_binary`, `rust_library`,
    /// `rust_proc_macro`, `cc_import`.
    pub rule: String,
    /// Crate root source file, relative to the workspace root.
    pub crate_root: Option<String>,
    /// Rust edition: `2015`, `2018`, `2021`, or `2024`.
    pub edition: Option<String>,
    /// Dependencies as labels (`:name`).
    #[serde(default)]
    pub deps: Vec<String>,
    /// Dev-dependencies as labels (`:name`) — used by `rust_test` targets.
    #[serde(default)]
    pub dev_deps: Vec<String>,
    /// Extra rustc flags.
    #[serde(default)]
    pub rustflags: Vec<String>,
    /// Per-target environment variables.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Features to activate on this target.
    #[serde(default)]
    pub features: Vec<String>,
    /// Whether the target's default feature is enabled.
    pub default_features: Option<bool>,
    /// Build-script source, relative to the workspace root.
    pub build_script: Option<String>,
    /// Crate types for a library target.
    #[serde(default)]
    pub crate_types: Vec<String>,
    /// Compile as a proc macro.
    pub proc_macro: Option<bool>,
    /// `cc_import`: path to the shared library to import.
    pub shared: Option<String>,
    /// `cc_import`: `-l` link name.
    pub link_name: Option<String>,
}

/// Manifest loading error.
#[derive(Debug)]
pub enum ManifestError {
    /// The file could not be read.
    Io(String, io::Error),
    /// The file was not valid TOML or failed schema validation.
    Parse(String, toml::de::Error),
}

impl std::fmt::Display for ManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(path, err) => write!(f, "cannot read {path}: {err}"),
            Self::Parse(path, err) => write!(f, "cannot parse {path}: {err}"),
        }
    }
}

impl std::error::Error for ManifestError {}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"
[workspace]
name = "hello"

[toolchain.rust]
kind = "system"

[profile.release]
opt_level = 3
lto = "thin"
panic = "abort"
codegen_units = 16
overflow_checks = false

[target.hello]
rule = "rust_binary"
crate_root = "src/main.rs"
edition = "2021"
deps = [":greet"]
rustflags = ["-D", "warnings"]
env = { GREETING = "hello" }
"#;

    #[test]
    fn parses_valid_manifest() {
        let manifest = Manifest::parse(VALID).unwrap();
        assert_eq!(manifest.workspace.name.as_deref(), Some("hello"));
        assert_eq!(manifest.toolchain.rust.kind, "system");
        let release = manifest.profile.get("release").unwrap();
        assert_eq!(release.opt_level.as_ref().unwrap().to_rustc(), "3");
        assert_eq!(release.lto, Some(Lto::Str("thin".to_owned())));
        assert_eq!(release.panic.as_deref(), Some("abort"));
        let hello = manifest.target.get("hello").unwrap();
        assert_eq!(hello.rule, "rust_binary");
        assert_eq!(hello.deps, vec![":greet"]);
        assert_eq!(hello.rustflags, vec!["-D", "warnings"]);
        assert_eq!(hello.env.get("GREETING").unwrap(), "hello");
    }

    #[test]
    fn rejects_unknown_fields() {
        let err = Manifest::parse("[workspace]\nname = \"x\"\nunknown = 1\n").unwrap_err();
        assert!(err.to_string().contains("unknown"), "{err}");
    }

    #[test]
    fn unknown_target_rules_are_a_backend_concern() {
        let manifest = Manifest::parse("[target.x]\nrule = \"future_rule\"\n").unwrap();
        assert_eq!(manifest.target["x"].rule, "future_rule");
    }
}
