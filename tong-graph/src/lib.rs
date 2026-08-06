//! Graph layer for Tong.
//!
//! Owns target labels, the `Tong.toml` manifest model, planned actions, and
//! scheduling. Depends on `tong-core` for the action and provider model and
//! on `tong-store` for the CAS used when assembling input roots; must not
//! depend on execution crates.

pub mod label;
pub mod manifest;
pub mod plan;

pub use label::{Label, LabelError};
pub use manifest::{
    Lto, Manifest, ManifestError, OptLevel, ProfileConfig, RustToolchain, TargetConfig, Toolchain,
    Workspace,
};
pub use plan::{Completed, CycleError, PlanError, PlannedAction, topological_order};
