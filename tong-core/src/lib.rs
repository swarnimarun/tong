//! Core data model for Tong.
//!
//! Owns the versioned action schema, artifact references, canonical digests,
//! platform definitions, providers, and the diagnostic format. See PLAN.md
//! sections 4-7 for the governing semantics.
//!
//! ## Canonical encoding and digests
//!
//! Every hashed structure uses the single canonical binary encoding in
//! [`canonical`] and SHA-256 digests from [`digest`] (PLAN.md section 4.5).
//! The encoding rules are documented on the [`canonical`] module; they are
//! the cross-platform cache-compatibility contract, so change them only with
//! a schema-version bump.

pub mod canonical;
pub mod digest;
