//! Storage layer for Tong.
//!
//! Owns the content-addressed store, action cache, build-state manifests, and
//! reachability-based garbage collection. Atomic CAS writes and per-digest
//! concurrency rules are defined in PLAN.md section 10.
