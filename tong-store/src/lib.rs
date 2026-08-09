//! Storage layer for Tong.
//!
//! Owns the content-addressed store, action cache, build-state manifests, and
//! reachability-based garbage collection. Atomic CAS writes and per-digest
//! concurrency rules are defined in PLAN.md section 10.

pub mod action_cache;
pub mod cas;
pub mod gc;
pub mod state;

pub use action_cache::{ActionCache, CachedResult};
pub use cas::{CAPTURE_EXCLUDES, Cas};
pub use gc::{GcOptions, GcReport, sweep};
pub use state::{
    BUILD_MANIFEST_SCHEMA_VERSION, BuildManifest, RecordedAction, StateStore, graph_digest,
    project_hash,
};
