//! The Rust backend: target model, toolchain capture, action planning.
//!
//! Lowers both `Tong.toml` targets and imported `Cargo.toml` manifests into
//! immutable compile actions (PLAN.md sections 3.1 and 8). Version 1 is
//! offline and workspace-local: path dependencies only, no registry, no
//! network; unsupported Cargo behavior fails with a targeted diagnostic.

pub mod backend;
pub mod build_directives;
pub mod cargo_import;
pub mod features;
pub mod model;
pub mod toolchain;
pub mod toolchain_dist;

pub use backend::{FinalArtifact, RustBackend};
pub use cargo_import::{
    CargoImportError, LockedSource, LockedSourceProvider, cargo_patch_names, import_cargo_workspace,
};
pub use features::{FeatureError, FeatureMap, FeatureRequest, resolve_features};
pub use model::{
    BinTarget, CcImport, CrateType, Dep, Edition, GitSelector, LibTarget, Lto, Package, PackageId,
    PackageMetadata, PanicStrategy, ProfileSpec, RegistryEdge, ResolverVersion, RustModel,
    RustUnitDomain, RustUnitId, RustUnitMode, SourceId, TestTarget,
};
pub use toolchain::{SystemRust, ToolchainError, capture_system_rust, host_triple};
pub use toolchain_dist::{dist_available, fetch_dist_rust, load_dist_rust};
