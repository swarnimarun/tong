//! Storage layer for Tong.
//!
//! Owns the content-addressed store (`cas`), the digest-keyed action cache
//! (`action_cache`), build-state manifests (`state`), and reachability-based
//! garbage collection (`gc`). Atomic CAS writes and per-digest concurrency
//! rules are defined in PLAN.md section 10; the GC root set, retention, and
//! shared-store rules are section 10.4.

pub mod action_cache;
pub mod cas;
pub mod gc;
pub mod state;

pub use action_cache::{ActionCache, CachedResult};
pub use cas::{CAPTURE_EXCLUDES, Cas, ClosureVerifier};
pub use gc::{GcOptions, GcReport, sweep};
pub use state::{
    BUILD_MANIFEST_SCHEMA_VERSION, BuildManifest, RecordedAction, StateStore, graph_digest,
    project_hash,
};
