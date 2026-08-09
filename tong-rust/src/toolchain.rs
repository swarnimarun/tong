//! System toolchain capture.
//!
//! Phase 2 uses the rustc installed on the host, captured as a *non-portable*
//! system bundle (PLAN.md section 5: "System toolchain capture for
//! local-only compatibility"): the rustc binary is imported into the store
//! for identity, the sysroot is fingerprinted (hashed, not imported), and
//! the bundle digest covers the whole closure. The rustup shim is never the
//! executable — the real binary under the sysroot is resolved and used.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use tong_core::action::CanonicalValue;
use tong_core::artifact::{BlobDigest, TreeDigest};
use tong_core::bundle::EnvironmentBundle;
use tong_core::platform::PlatformKey;
use tong_store::Cas;

/// A captured system Rust toolchain.
#[derive(Clone, Debug)]
pub struct SystemRust {
    /// Toolchain root (the sysroot).
    pub root: PathBuf,
    /// The real rustc binary (never a rustup shim).
    pub rustc: PathBuf,
    /// Host target triple.
    pub host_triple: String,
    /// Full `rustc -vV` output, used as an action property.
    pub version_verbose: String,
    /// Digest of the rustc binary itself.
    pub rustc_blob: BlobDigest,
    /// Fingerprint of the sysroot's `bin/` and `lib/` trees.
    pub sysroot_tree: TreeDigest,
    /// The non-portable environment bundle.
    pub bundle: EnvironmentBundle,
}

impl SystemRust {
    /// Bundle reference for action specs.
    pub fn bundle_ref(&self) -> tong_core::bundle::BundleRef {
        tong_core::bundle::BundleRef::of(&self.bundle)
    }
}

/// Toolchain capture failure.
#[derive(Debug)]
pub enum ToolchainError {
    /// rustc could not be found or run.
    Missing(String),
    /// The real rustc binary does not exist under the reported sysroot.
    InvalidSysroot(PathBuf),
    /// Store failure.
    Io(std::io::Error),
    /// Downloaded-toolchain failure (rustup dist protocol).
    Dist(String),
}

impl std::fmt::Display for ToolchainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing(msg) => write!(f, "cannot locate Rust toolchain: {msg}"),
            Self::InvalidSysroot(root) => write!(
                f,
                "rustc reported sysroot {} but no rustc binary exists there",
                root.display()
            ),
            Self::Io(err) => write!(f, "toolchain capture failed: {err}"),
            Self::Dist(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for ToolchainError {}

impl From<std::io::Error> for ToolchainError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

/// Returns the host target triple by querying rustc (`rustc -vV`). Cheap
/// (one process); used before model import when the full toolchain capture
/// has not run yet.
pub fn host_triple() -> Result<String, ToolchainError> {
    let rustc = std::env::var_os("TONG_RUSTC")
        .map(PathBuf::from)
        .or_else(|| find_on_path("rustc"))
        .ok_or_else(|| ToolchainError::Missing("no rustc on PATH".to_owned()))?;
    let version_verbose = run_toolchain(&rustc, &["-vV"])?;
    version_verbose
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .map(str::trim)
        .map(str::to_owned)
        .ok_or_else(|| ToolchainError::Missing("rustc -vV reported no host triple".to_owned()))
}

/// Captures the system Rust toolchain into the store.
pub fn capture_system_rust(cas: &Cas) -> Result<SystemRust, ToolchainError> {
    // 1. Locate rustc. TONG_RUSTC overrides PATH for testing.
    let rustc = std::env::var_os("TONG_RUSTC")
        .map(PathBuf::from)
        .or_else(|| find_on_path("rustc"))
        .ok_or_else(|| ToolchainError::Missing("no rustc on PATH".to_owned()))?;

    // 2. Ask rustc for its sysroot and verbose version.
    let sysroot = run_toolchain(&rustc, &["--print", "sysroot"])?;
    let version_verbose = run_toolchain(&rustc, &["-vV"])?;
    let host_triple = version_verbose
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .map(str::trim)
        .ok_or_else(|| ToolchainError::Missing("rustc -vV reported no host triple".to_owned()))?
        .to_owned();

    let root = PathBuf::from(sysroot.trim());
    let real_rustc = root.join("bin").join("rustc");
    if !real_rustc.is_file() {
        return Err(ToolchainError::InvalidSysroot(root));
    }

    // 3. Import the real rustc binary for identity; fingerprint the
    //    sysroot's bin/ and lib/ trees (the closure rustc reads).
    let rustc_blob = cas.put_file(&real_rustc)?;
    let excludes = Default::default();
    let bin_tree = cas.fingerprint_dir(&root.join("bin"), &excludes)?;
    let lib_tree = cas.fingerprint_dir(&root.join("lib"), &excludes)?;
    let sysroot_tree = cas.assemble(&[
        (
            tong_core::paths::RelativePath::new("bin").unwrap(),
            bin_tree,
        ),
        (
            tong_core::paths::RelativePath::new("lib").unwrap(),
            lib_tree,
        ),
    ])?;

    // 4. Build the non-portable bundle (PLAN.md section 5).
    let version = first_line_of(&version_verbose).unwrap_or("unknown");
    let bundle = EnvironmentBundle {
        name: format!("system-rustc-{version}-{host_triple}"),
        provider: "system-capture".to_owned(),
        platform: host_platform(),
        variables: BTreeMap::new(),
        files: sysroot_tree,
        metadata: BTreeMap::from([
            (
                "tong.rust.rustc_verbose_version".to_owned(),
                CanonicalValue::String(version_verbose.trim().to_owned()),
            ),
            (
                "tong.rust.host_triple".to_owned(),
                CanonicalValue::String(host_triple.trim().to_owned()),
            ),
        ]),
    };
    cas.put_bundle(&bundle)?;

    Ok(SystemRust {
        root,
        rustc: real_rustc,
        host_triple: host_triple.trim().to_owned(),
        version_verbose: version_verbose.trim().to_owned(),
        rustc_blob,
        sysroot_tree,
        bundle,
    })
}

/// Runs a toolchain binary and returns stdout.
pub fn run_toolchain(program: &Path, args: &[&str]) -> Result<String, ToolchainError> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|err| ToolchainError::Missing(format!("{}: {err}", program.display())))?;
    if !output.status.success() {
        return Err(ToolchainError::Missing(format!(
            "{} {args:?} failed: {}",
            program.display(),
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn first_line_of(text: &str) -> Option<&str> {
    text.lines().next().map(str::trim)
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// The host platform as a constraint key.
pub fn host_platform() -> PlatformKey {
    PlatformKey::from_pairs(&[
        ("os", std::env::consts::OS),
        ("arch", std::env::consts::ARCH),
    ])
}

/// The shared-library extension of the host platform (rustc output files).
pub fn dll_extension() -> &'static str {
    std::env::consts::DLL_EXTENSION
}
