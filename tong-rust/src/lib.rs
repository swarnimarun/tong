//! The Rust backend: target model, toolchain capture, action planning.
//!
//! Lowers both `Tong.toml` targets and imported `Cargo.toml` manifests into
//! immutable compile actions (PLAN.md sections 3.1 and 8). Version 1 is
//! offline and workspace-local: path dependencies only, no registry, no
//! network; unsupported Cargo behavior fails with a targeted diagnostic.

pub mod backend;
pub mod build_directives;
pub mod model;
pub mod toolchain;

pub use backend::{FinalArtifact, RustBackend};
pub use model::{
    BinTarget, CcImport, CrateType, Dep, Edition, LibTarget, Lto, Package, PanicStrategy,
    ProfileSpec, RustModel,
};
pub use toolchain::{SystemRust, ToolchainError, capture_system_rust};
