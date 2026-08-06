//! Graph layer for Tong.
//!
//! Owns target labels, the configured target graph, pure analysis (PLAN.md
//! section 3.2), and the scheduler. Depends on `tong-core` for the action and
//! provider model; must not depend on execution or storage crates.
