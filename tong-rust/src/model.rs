//! The Rust backend target model.
//!
//! Both `Tong.toml` targets and imported `Cargo.toml` manifests lower into
//! this model; the backend plans actions from it. The model is deliberately
//! close to Cargo's package model because Rust's compiler is package-based.

use std::collections::BTreeMap;
use std::path::PathBuf;

/// The full set of Rust targets to build.
#[derive(Clone, Debug, Default)]
pub struct RustModel {
    /// Workspace packages.
    pub packages: Vec<Package>,
    /// Named, resolved profiles.
    pub profiles: BTreeMap<String, ProfileSpec>,
    /// Imported prebuilt native libraries.
    pub cc_imports: Vec<CcImport>,
    /// Workspace-wide rustc flags (e.g. `.cargo/config.toml` `[build]`).
    pub global_rustflags: Vec<String>,
    /// Workspace-wide environment (e.g. `.cargo/config.toml` `[env]`).
    pub global_env: BTreeMap<String, String>,
}

/// A Rust package (one crate compilation unit).
#[derive(Clone, Debug)]
pub struct Package {
    /// Package name (may contain hyphens).
    pub name: String,
    /// Source root directory, relative to the workspace root.
    pub dir: PathBuf,
    /// Package version.
    pub version: String,
    /// Rust edition.
    pub edition: Edition,
    /// Library target, if any (or proc macro).
    pub lib: Option<LibTarget>,
    /// Binary targets.
    pub bins: Vec<BinTarget>,
    /// Build script, relative to `dir`.
    pub build_script: Option<PathBuf>,
    /// Normal dependencies.
    pub deps: Vec<Dep>,
    /// Build-script-only dependencies.
    pub build_deps: Vec<Dep>,
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
    /// Crate root, relative to `dir`.
    pub path: PathBuf,
}

/// A dependency on another package in the workspace.
#[derive(Clone, Debug)]
pub struct Dep {
    /// `--extern` name used by the dependent crate.
    pub extern_name: String,
    /// Name of the dependency package.
    pub package: String,
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

/// A resolved build profile (PLAN.md section 8.5).
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
    /// Overflow checks.
    pub overflow_checks: bool,
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
            overflow_checks: true,
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
            overflow_checks: false,
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
        flags.push("-C".to_owned());
        flags.push(format!(
            "overflow-checks={}",
            if self.overflow_checks { "on" } else { "off" }
        ));
        if let Some(units) = self.codegen_units {
            flags.push("-C".to_owned());
            flags.push(format!("codegen-units={units}"));
        }
        flags
    }
}
