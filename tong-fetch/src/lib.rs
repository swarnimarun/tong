//! Registry fetching: sparse index, version resolution, crate downloads.
//!
//! Network happens only here, and only through [`registry::fetch_url`]
//! (called from `tong lock` / `tong fetch`); builds are always offline
//! against `Tong.lock`. All index access is through the sparse protocol
//! (Cargo book "Registry Index" / "Sparse Registry"); the index cache is a
//! plain, mutable cache (not CAS content).

pub mod download;
pub mod lockfile;
pub mod registry;
pub mod resolve;
pub mod sparse_index;

pub use download::{crate_blob_path, fetch_crate, materialize_source, verify_crate_bytes};
pub use lockfile::{LOCKFILE_VERSION, LockedPackage, TongLock};
pub use registry::{FetchError, RegistryConfig, fetch_url};
pub use resolve::{CrateSource, LocalPackage, ResolveError, ResolvedDep, ResolvedPackage, resolve};
pub use sparse_index::{IndexClient, IndexDep, IndexVersion};
