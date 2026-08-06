//! `Cargo.toml` import mode (PLAN.md section 8.1).
//!
//! Cargo manifests are translated into the same [`RustModel`] the native
//! `Tong.toml` targets produce; Cargo is never invoked during a Tong build.
//! Version 1 supports offline workspace-local projects: path dependencies
//! only. Registry and git dependencies fail with a targeted diagnostic —
//! locked fetching is Phase 3 (PLAN.md section 9) and must not silently
//! change semantics (section 15, Phase 5 exit criteria).

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::model::{
    BinTarget, Dep, Edition, LibTarget, Lto, Package, PanicStrategy, ProfileSpec, RustModel,
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
    lib: Option<CargoLib>,
    #[serde(default)]
    bin: Vec<CargoBin>,
    #[serde(default)]
    profile: BTreeMap<String, CargoProfile>,
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
struct CargoPackage {
    name: String,
    #[serde(default = "default_version")]
    version: String,
    #[serde(default = "default_edition")]
    edition: String,
    build: Option<String>,
}

fn default_version() -> String {
    "0.0.0".to_owned()
}

fn default_edition() -> String {
    "2015".to_owned()
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
struct CargoWorkspace {
    #[serde(default)]
    members: Vec<String>,
    #[serde(default)]
    dependencies: BTreeMap<String, DepValue>,
}

#[derive(Deserialize, Clone)]
#[serde(untagged)]
enum DepValue {
    /// `name = "0.1"` — registry dependency.
    Version(String),
    /// `name = { path = "..." }` or `{ workspace = true }`.
    Table {
        path: Option<String>,
        #[serde(rename = "workspace")]
        workspace: Option<bool>,
    },
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
struct CargoLib {
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

#[derive(Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
struct CargoProfile {
    opt_level: Option<OptLevelValue>,
    debug: Option<DebugValue>,
    lto: Option<LtoValue>,
    panic: Option<String>,
    codegen_units: Option<u32>,
    overflow_checks: Option<bool>,
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

/// Imports a Cargo workspace into a [`RustModel`].
pub fn import_cargo_workspace(workspace_root: &Path) -> Result<RustModel, CargoImportError> {
    let root_manifest = read_manifest(workspace_root)?;

    // Workspace dependencies for inheritance.
    let mut inherited: BTreeMap<String, DepValue> = BTreeMap::new();
    if let Some(workspace) = &root_manifest.workspace {
        inherited.extend(workspace.dependencies.clone());
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

    for member in &members {
        let manifest = read_manifest(member)?;
        let package = manifest.package.as_ref().ok_or_else(|| {
            CargoImportError::Unsupported(format!(
                "{} declares a workspace but not a package",
                member.display()
            ))
        })?;

        let dir = member.clone();
        let mut pkg = Package {
            name: package.name.clone(),
            dir,
            version: package.version.clone(),
            edition: parse_edition(&package.edition)?,
            lib: None,
            bins: Vec::new(),
            build_script: package.build.as_ref().map(PathBuf::from),
            deps: Vec::new(),
            build_deps: Vec::new(),
            rustflags: Vec::new(),
            env: BTreeMap::new(),
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
                crate_types: parse_crate_types(&lib.crate_type)?,
                proc_macro: lib.proc_macro,
                path: lib_path,
            });
        } else if lib_present {
            pkg.lib = Some(LibTarget {
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

        // Dependencies (path and workspace-inherited only).
        pkg.deps = resolve_deps(&manifest.dependencies, member, &inherited)?;
        pkg.build_deps = resolve_deps(&manifest.build_dependencies, member, &inherited)?;

        model.packages.push(pkg);
    }

    // Profiles from the workspace root manifest (Cargo: [profile.*] tables).
    model.profiles = resolve_profiles(&root_manifest.profile);
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

fn resolve_deps(
    deps: &BTreeMap<String, DepValue>,
    member: &Path,
    inherited: &BTreeMap<String, DepValue>,
) -> Result<Vec<Dep>, CargoImportError> {
    let mut out = Vec::new();
    for (name, value) in deps {
        let resolved = match value {
            DepValue::Version(version) => {
                return Err(CargoImportError::Unsupported(format!(
                    "dependency {name:?} = {version:?} in {} is a registry dependency; \
                     Tong offline mode requires path or workspace dependencies",
                    member.display()
                )));
            }
            DepValue::Table { path, workspace } => {
                if let Some(path) = path {
                    Some((name.clone(), PathBuf::from(path)))
                } else if *workspace == Some(true) {
                    match inherited.get(name) {
                        Some(DepValue::Table {
                            path: Some(path), ..
                        }) => Some((name.clone(), PathBuf::from(path))),
                        _ => {
                            return Err(CargoImportError::Unsupported(format!(
                                "dependency {name:?} uses workspace inheritance without a \
                                 path dependency in [workspace.dependencies]"
                            )));
                        }
                    }
                } else {
                    return Err(CargoImportError::Unsupported(format!(
                        "dependency {name:?} in {} is neither a path nor workspace \
                         dependency",
                        member.display()
                    )));
                }
            }
        };
        if let Some((extern_name, _path)) = resolved {
            // The package name is the dep key; the target dir is used to
            // validate, but workspace-local resolution means the package
            // name is the same as the crate name.
            out.push(Dep {
                extern_name: extern_name.replace('-', "_"),
                package: extern_name,
            });
        }
    }
    Ok(out)
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

fn resolve_profiles(tables: &BTreeMap<String, CargoProfile>) -> BTreeMap<String, ProfileSpec> {
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
            spec.overflow_checks = checks;
        }
        out.insert(name.clone(), spec);
    }
    out
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

    fn write_tree(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (path, content) in files {
            let full = dir.path().join(path);
            fs::create_dir_all(full.parent().unwrap()).unwrap();
            fs::write(full, content).unwrap();
        }
        dir
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
        let model = import_cargo_workspace(dir.path()).unwrap();
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
        let err = import_cargo_workspace(dir.path()).unwrap_err();
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
        let model = import_cargo_workspace(dir.path()).unwrap();
        assert_eq!(model.global_rustflags, vec!["--cfg", "advanced_mode"]);
        assert_eq!(model.global_env.get("APP_GREETING").unwrap(), "hello");
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
        let model = import_cargo_workspace(dir.path()).unwrap();
        let release = model.profiles.get("release").unwrap();
        assert_eq!(release.opt_level, "2");
        assert_eq!(release.lto, Lto::Thin);
        assert_eq!(release.panic, PanicStrategy::Abort);
        assert_eq!(release.codegen_units, Some(4));
        assert!(!release.overflow_checks);
    }
}
