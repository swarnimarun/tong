//! `Tong.toml` native targets → Rust model.
//!
//! The graph layer models the manifest file; this module interprets its
//! targets for the Rust backend. Cargo-style auto-detection (src/lib.rs,
//! src/main.rs) applies so native manifests stay small.

use std::path::{Path, PathBuf};

use tong_graph::manifest::{Lto, Manifest, OptLevel, ProfileConfig};
use tong_rust::model::{
    BinTarget, CcImport, Dep, Edition, LibTarget, Package, ProfileSpec, RustModel,
};

/// Converts a native `Tong.toml` manifest into the Rust model.
pub fn manifest_to_model(manifest: &Manifest, root: &std::path::Path) -> RustModel {
    let mut model = RustModel::default();

    for (name, target) in &manifest.target {
        match target.rule.as_str() {
            "rust_library" | "rust_proc_macro" => {
                let proc_macro = target
                    .proc_macro
                    .unwrap_or(target.rule == "rust_proc_macro");
                let path = target
                    .crate_root
                    .clone()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("src/lib.rs"));
                let crate_types = parse_crate_types(&target.crate_types);
                model.packages.push(Package {
                    name: name.clone(),
                    dir: root.to_path_buf(),
                    version: "0.0.0".to_owned(),
                    edition: parse_edition(target.edition.as_deref()),
                    lib: Some(LibTarget {
                        name: None,
                        crate_types,
                        proc_macro,
                        path,
                    }),
                    bins: Vec::new(),
                    build_script: target.build_script.as_ref().map(PathBuf::from),
                    deps: parse_deps(&target.deps),
                    build_deps: Vec::new(),
                    rustflags: target.rustflags.clone(),
                    env: target.env.clone(),
                });
            }
            "rust_binary" => {
                let path = target
                    .crate_root
                    .clone()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("src/main.rs"));
                let mut pkg = Package {
                    name: name.clone(),
                    dir: root.to_path_buf(),
                    version: "0.0.0".to_owned(),
                    edition: parse_edition(target.edition.as_deref()),
                    lib: None,
                    bins: vec![BinTarget {
                        name: name.clone(),
                        path,
                    }],
                    build_script: target.build_script.as_ref().map(PathBuf::from),
                    deps: parse_deps(&target.deps),
                    build_deps: Vec::new(),
                    rustflags: target.rustflags.clone(),
                    env: target.env.clone(),
                };
                // Cargo-style auto library next to the binary.
                if root.join("src/lib.rs").is_file() {
                    pkg.lib = Some(LibTarget {
                        name: None,
                        crate_types: Vec::new(),
                        proc_macro: false,
                        path: PathBuf::from("src/lib.rs"),
                    });
                }
                model.packages.push(pkg);
            }
            "cc_import" => {
                let shared = target.shared.clone().map(PathBuf::from).unwrap_or_default();
                let shared = if shared.is_absolute() {
                    shared
                } else {
                    root.join(shared)
                };
                let link_name = target
                    .link_name
                    .clone()
                    .unwrap_or_else(|| default_link_name(&shared));
                model.cc_imports.push(CcImport {
                    name: name.clone(),
                    shared,
                    link_name,
                });
            }
            other => {
                eprintln!("tong: warning: ignoring target {name:?} with unknown rule {other:?}");
            }
        }
    }

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

    model
}

fn parse_deps(deps: &[String]) -> Vec<Dep> {
    deps.iter()
        .filter_map(|label| {
            let name = match tong_graph::Label::parse(label) {
                Ok(label) if label.package.is_empty() => label.name,
                Ok(_) => return None,
                Err(_) => return None,
            };
            Some(Dep {
                extern_name: name.replace('-', "_"),
                package: name,
            })
        })
        .collect()
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
