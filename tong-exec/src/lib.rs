//! Execution layer for Tong.
//!
//! Owns the local and process executors, sandbox launchers, action-result
//! validation (PLAN.md section 4.6), and the structured event log. Sandboxing
//! levels are defined in PLAN.md section 11.

pub mod local;
pub mod sandbox;

pub use local::{BUNDLE_ROOT_VAR, EXEC_ROOT_VAR, ExecError, ExecOutcome, LocalExecutor};
pub use sandbox::{SandboxLevel, SandboxSpec, sandbox_for};
