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
    #[serde(default)]
    version: Option<Field>,
    #[serde(default)]
    edition: Option<Field>,
    build: Option<String>,
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
            &mut packages,
            &mut visiting,
            &inherited,
            workspace_root,
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
    model.packages = packages.into_values().collect();

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

/// Imports the package at `dir` (a workspace member or a path dependency)
/// into `packages`, recursing into its path dependencies. Returns the
/// package's declared name. Cycles are rejected, matching Cargo.
fn import_package(
    dir: &Path,
    packages: &mut BTreeMap<PathBuf, Package>,
    visiting: &mut Vec<PathBuf>,
    inherited: &Inherited,
    workspace_root: &Path,
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
            build_script: None,
            deps: Vec::new(),
            build_deps: Vec::new(),
            rustflags: Vec::new(),
            env: BTreeMap::new(),
        };
        // Cargo auto-detects build.rs at the package root when the `build`
        // key is absent.
        pkg.build_script =
            package.build.as_ref().map(PathBuf::from).or_else(|| {
                (pkg.dir.join("build.rs").is_file()).then(|| PathBuf::from("build.rs"))
            });

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

        // Dependencies (path and workspace-inherited only). Direct paths
        // resolve relative to this manifest; inherited paths resolve
        // relative to the workspace root.
        let resolved_deps = resolve_deps(
            &manifest.dependencies,
            &canonical,
            workspace_root,
            &inherited.deps,
        )?;
        let resolved_build_deps = resolve_deps(
            &manifest.build_dependencies,
            &canonical,
            workspace_root,
            &inherited.deps,
        )?;
        let mut deps = Vec::new();
        let mut build_deps = Vec::new();
        for (resolved, target) in [
            (resolved_deps, &mut deps),
            (resolved_build_deps, &mut build_deps),
        ] {
            for dep in resolved {
                let real_name = match dep.path {
                    Some(path) => {
                        let imported =
                            import_package(&path, packages, visiting, &inherited, workspace_root)?;
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
                    None => dep.package,
                };
                target.push(Dep {
                    extern_name: dep.extern_name,
                    package: real_name,
                });
            }
        }
        pkg.deps = deps;
        pkg.build_deps = build_deps;

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
/// name it refers to, and — for path dependencies — the package directory.
struct ResolvedDep {
    extern_name: String,
    package: String,
    path: Option<PathBuf>,
}

fn resolve_deps(
    deps: &BTreeMap<String, DepValue>,
    member: &Path,
    workspace_root: &Path,
    inherited: &BTreeMap<String, DepValue>,
) -> Result<Vec<ResolvedDep>, CargoImportError> {
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
            DepValue::Table {
                path,
                version,
                workspace,
                package,
            } => {
                if let Some(path) = path {
                    // Path deps resolve relative to the declaring manifest.
                    let dir = member.join(path);
                    Some((
                        name.clone(),
                        package.clone().unwrap_or_else(|| name.clone()),
                        Some(dir),
                    ))
                } else if *workspace == Some(true) {
                    match inherited.get(name) {
                        Some(DepValue::Table {
                            path: Some(path),
                            package: inherited_package,
                            ..
                        }) => {
                            // Inherited path deps resolve relative to the
                            // workspace root manifest.
                            let dir = workspace_root.join(path);
                            Some((
                                name.clone(),
                                inherited_package.clone().unwrap_or_else(|| name.clone()),
                                Some(dir),
                            ))
                        }
                        Some(DepValue::Version(version)) => {
                            return Err(CargoImportError::Unsupported(format!(
                                "dependency {name:?} = {version:?} in {} is inherited \
                                 from [workspace.dependencies] and is a registry \
                                 dependency; Tong offline mode requires path or \
                                 workspace dependencies",
                                member.display()
                            )));
                        }
                        Some(DepValue::Table {
                            version: Some(version),
                            ..
                        }) => {
                            return Err(CargoImportError::Unsupported(format!(
                                "dependency {name:?} = {version:?} in {} is inherited \
                                 from [workspace.dependencies] and is a registry \
                                 dependency; Tong offline mode requires path or \
                                 workspace dependencies",
                                member.display()
                            )));
                        }
                        _ => {
                            return Err(CargoImportError::Unsupported(format!(
                                "dependency {name:?} uses workspace inheritance without a \
                                 path dependency in [workspace.dependencies]"
                            )));
                        }
                    }
                } else if let Some(version) = version {
                    return Err(CargoImportError::Unsupported(format!(
                        "dependency {name:?} = {version:?} in {} is a registry \
                         dependency; Tong offline mode requires path or workspace \
                         dependencies",
                        member.display()
                    )));
                } else {
                    return Err(CargoImportError::Unsupported(format!(
                        "dependency {name:?} in {} is neither a path nor workspace \
                         dependency",
                        member.display()
                    )));
                }
            }
        };
        if let Some((extern_name, package, path)) = resolved {
            out.push(ResolvedDep {
                extern_name: extern_name.replace('-', "_"),
                package,
                path,
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
        let model = import_cargo_workspace(dir.path()).unwrap();
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
            fs::canonicalize(dir.path().join("shared/sdl3-sys")).unwrap()
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
        let err = import_cargo_workspace(dir.path()).unwrap_err();
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
        let model = import_cargo_workspace(dir.path()).unwrap();
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
        let err = import_cargo_workspace(dir.path()).unwrap_err();
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
        let model = import_cargo_workspace(dir.path()).unwrap();
        let app = model.packages.iter().find(|p| p.name == "app").unwrap();
        assert_eq!(app.lib.as_ref().unwrap().name.as_deref(), Some("app_core"));
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
