//! System toolchain capture.
//!
//! Phase 2 uses the rustc installed on the host, captured as a *non-portable*
//! system bundle (PLAN.md section 5: "System toolchain capture for
//! local-only compatibility"): the rustc binary is imported into the store
//! for identity, the sysroot is fingerprinted (hashed, not imported), and
//! the bundle digest covers the whole closure. The rustup shim is never the
//! executable — the real binary under the sysroot is resolved and used.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use tong_core::action::CanonicalValue;
use tong_core::artifact::{BlobDigest, TreeDigest};
use tong_core::bundle::{ENVIRONMENT_BUNDLE_SCHEMA_VERSION, EnvironmentBundle};
use tong_core::canonical::{CanonicalDecode, CanonicalEncode, Decoder, Encoder};
use tong_core::digest::{Digest, Hasher};
use tong_core::platform::PlatformKey;
use tong_core::tree::{TREE_SCHEMA_VERSION, Tree};
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

    // 2. Ask rustc for its sysroot (one spawn — the rustup shim is slow).
    //    The verbose version and host triple are only needed when the
    //    capture cache misses, and are then read from the real binary
    //    under the sysroot — the binary that actually executes.
    let sysroot = run_toolchain(&rustc, &["--print", "sysroot"])?;
    let root = PathBuf::from(sysroot.trim());
    let real_rustc = root.join("bin").join("rustc");
    if !real_rustc.is_file() {
        return Err(ToolchainError::InvalidSysroot(root));
    }

    // 3. Reuse the per-machine capture cache while the sysroot stat
    //    snapshot is unchanged (docs/fingerprint-cache.md). Cached digests
    //    come from a full content hash; the snapshot only decides whether
    //    that hash is still valid — never what the digests are. The key is
    //    the sysroot path alone: a changed toolchain at the same path
    //    changes the snapshot, and the stored version/identity strings
    //    belong to the snapshot that was verified.
    let cache = ToolchainCache::open();
    let cache_lookup = cache.as_ref().map(|cache| {
        let t_snapshot = std::time::Instant::now();
        let key = ToolchainCache::key(&root)?;
        let snapshot = ToolchainCache::snapshot(&root)?;
        tracing::debug!(
            target: "tong::perf",
            phase = "toolchain.cache.snapshot",
            duration_ms = t_snapshot.elapsed().as_millis() as u64,
        );
        Ok::<_, ToolchainError>((cache, key, snapshot))
    });
    if let Some(Ok((cache, key, snapshot))) = cache_lookup.as_ref() {
        let t_load = std::time::Instant::now();
        match cache.load(cas, key, snapshot) {
            Ok(Some(captured)) => {
                tracing::debug!(
                    target: "tong::perf",
                    phase = "toolchain.cache.load",
                    duration_ms = t_load.elapsed().as_millis() as u64,
                );
                tracing::debug!(target: "tong::perf", phase = "toolchain.cache", hit = true);
                return Ok(SystemRust {
                    root,
                    rustc: real_rustc,
                    host_triple: captured.host_triple,
                    version_verbose: captured.version_verbose,
                    rustc_blob: captured.rustc_blob,
                    sysroot_tree: captured.sysroot_tree,
                    bundle: captured.bundle,
                });
            }
            Ok(None) => {
                tracing::debug!(target: "tong::perf", phase = "toolchain.cache", hit = false);
            }
            Err(err) => {
                tracing::debug!(
                    target: "tong::perf",
                    phase = "toolchain.cache",
                    hit = false,
                    error = %err,
                );
            }
        }
    }

    // 4. Full capture: query the real binary for its verbose version, then
    //    import it for identity and fingerprint the sysroot's bin/ and
    //    lib/ trees (the closure rustc reads).
    let t_capture = std::time::Instant::now();
    let version_verbose = run_toolchain(&real_rustc, &["-vV"])?;
    let host_triple = version_verbose
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .map(str::trim)
        .ok_or_else(|| ToolchainError::Missing("rustc -vV reported no host triple".to_owned()))?
        .to_owned();
    let rustc_blob = cas.put_file(&real_rustc)?;
    tracing::debug!(
        target: "tong::perf",
        phase = "toolchain.capture.rustc_blob",
        duration_ms = t_capture.elapsed().as_millis() as u64,
    );
    let excludes = Default::default();
    let bin_tree = cas.fingerprint_dir(&root.join("bin"), &excludes)?;
    tracing::debug!(
        target: "tong::perf",
        phase = "toolchain.capture.bin_tree",
        duration_ms = t_capture.elapsed().as_millis() as u64,
    );
    let lib_tree = cas.fingerprint_dir(&root.join("lib"), &excludes)?;
    tracing::debug!(
        target: "tong::perf",
        phase = "toolchain.capture.lib_tree",
        duration_ms = t_capture.elapsed().as_millis() as u64,
    );
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

    // 5. Populate the cache (best-effort: a cache failure only skips the
    //    optimization; the capture itself is already complete and stored).
    if let Some(Ok((cache, key, _))) = cache_lookup.as_ref() {
        match ToolchainCache::snapshot(&root) {
            Ok(snapshot) => {
                if let Err(err) = cache.store(
                    cas,
                    key,
                    &snapshot,
                    rustc_blob,
                    bin_tree,
                    lib_tree,
                    sysroot_tree,
                    &bundle,
                    &host_triple,
                    &version_verbose,
                ) {
                    tracing::debug!(
                        target: "tong::perf",
                        phase = "toolchain.cache.store",
                        error = %err,
                    );
                }
            }
            Err(err) => tracing::debug!(
                target: "tong::perf",
                phase = "toolchain.cache.store",
                error = %err,
            ),
        }
    }

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

/// Per-machine cache of system toolchain captures
/// (docs/fingerprint-cache.md).
///
/// The full content capture (rustc blob, sysroot tree digests, bundle) is
/// computed once and reused while a per-file stat snapshot of the sysroot
/// proves the toolchain unchanged. The cache lives outside the project so
/// `tong clean` does not destroy it. Objects are digest-named and
/// re-verified on restore, so a missing, stale, or corrupt entry degrades
/// to a full re-capture — never to a wrong toolchain identity.
pub struct ToolchainCache {
    dir: PathBuf,
}

/// A restored capture, with all objects re-imported into the project CAS.
struct CachedCapture {
    rustc_blob: BlobDigest,
    sysroot_tree: TreeDigest,
    bundle: EnvironmentBundle,
    host_triple: String,
    version_verbose: String,
}

/// Stat data of one sysroot entry as folded into the snapshot digest.
enum SnapshotEntry {
    File { mtime_ns: u64, size: u64, mode: u32 },
    Symlink { target: String },
}

/// File stat data: (mtime_ns, size, mode).
type FileStat = (u64, u64, u32);

/// Process-unique suffix for atomic temp files (pid + counter).
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Entry-count cap for the per-machine capture cache: pruning evicts the
/// oldest entries beyond this many (docs/fingerprint-cache.md).
const MAX_ENTRIES: usize = 64;
/// Total size cap (bytes) for the per-machine capture cache.
const MAX_BYTES: u64 = 512 * 1024 * 1024;

impl ToolchainCache {
    /// Opens the user-level cache: `$TONG_CACHE_DIR`, else
    /// `$HOME/.cache/tong`. `None` when no location is available.
    pub fn open() -> Option<Self> {
        let base = std::env::var_os("TONG_CACHE_DIR")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache").join("tong"))
            })?;
        Some(Self {
            dir: base.join("toolchains"),
        })
    }

    /// Cache key: the canonical sysroot path. A changed toolchain at the
    /// same path changes the snapshot (bin/ includes the rustc binary), so
    /// entries can never alias stale identity strings.
    fn key(root: &Path) -> io::Result<Digest> {
        let root = fs::canonicalize(root)?;
        Ok(Hasher::digest(root.to_string_lossy().as_bytes()))
    }

    /// Snapshot digest of the sysroot `bin/` and `lib/` trees: per-file
    /// (relpath, mtime_ns, size, mode) and symlink targets, canonically
    /// encoded and hashed. Every real toolchain change (rustup, brew,
    /// manual edits) alters this digest.
    fn snapshot(root: &Path) -> io::Result<Digest> {
        // Pass 1: walk, collecting file paths and symlink targets (no
        // metadata yet).
        let mut files: Vec<(String, PathBuf)> = Vec::new();
        let mut symlinks: Vec<(String, String)> = Vec::new();
        for sub in ["bin", "lib"] {
            collect_snapshot_paths(&root.join(sub), sub, &mut files, &mut symlinks)?;
        }
        // Pass 2: stat the files in parallel (thousands of sysroot files;
        // serial metadata dominates the snapshot otherwise).
        let mut entries: Vec<(String, SnapshotEntry)> =
            Vec::with_capacity(files.len() + symlinks.len());
        for (rel, stat) in stat_files_parallel(&files)? {
            entries.push((
                rel,
                SnapshotEntry::File {
                    mtime_ns: stat.0,
                    size: stat.1,
                    mode: stat.2,
                },
            ));
        }
        for (rel, target) in symlinks {
            entries.push((rel, SnapshotEntry::Symlink { target }));
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        let mut enc = Encoder::new();
        enc.write_u64(entries.len() as u64);
        for (rel, entry) in entries {
            rel.encode(&mut enc);
            match entry {
                SnapshotEntry::File {
                    mtime_ns,
                    size,
                    mode,
                } => {
                    enc.write_u8(0);
                    enc.write_u64(mtime_ns);
                    enc.write_u64(size);
                    enc.write_u32(mode);
                }
                SnapshotEntry::Symlink { target } => {
                    enc.write_u8(1);
                    target.encode(&mut enc);
                }
            }
        }
        Ok(enc.digest())
    }

    fn entry_dir(&self, key: &Digest) -> PathBuf {
        self.dir.join(key.to_hex())
    }

    /// Restores a cached capture into `cas`. `Ok(None)` means missing,
    /// stale (snapshot mismatch), or corrupt — never a wrong toolchain
    /// identity.
    fn load(
        &self,
        cas: &Cas,
        key: &Digest,
        snapshot: &Digest,
    ) -> io::Result<Option<CachedCapture>> {
        let dir = self.entry_dir(key);
        let snapshot_path = dir.join("snapshot");
        if !snapshot_path.is_file() {
            return Ok(None);
        }
        if fs::read_to_string(&snapshot_path)?.trim() != snapshot.to_hex() {
            return Ok(None);
        }
        let manifest = match fs::read_to_string(dir.join("capture")) {
            Ok(text) => text,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err),
        };
        let Some((rustc_blob, bin_tree, lib_tree, sysroot_tree, bundle_digest)) =
            parse_manifest(&manifest)
        else {
            return Ok(None);
        };

        // Restore every object. Each `put_*` recomputes the digest from the
        // bytes, so a corrupt file fails the equality check and degrades to
        // a miss instead of a wrong identity. Objects already present are
        // skipped: a content-addressed store needs no re-verification.
        let Some(rustc_bytes) = read_cached(&dir, "blob", rustc_blob.digest())? else {
            return Ok(None);
        };
        if cas.blob_path(rustc_blob).is_none() {
            match cas.put_blob(&rustc_bytes) {
                Ok(got) if got == rustc_blob => {}
                _ => return Ok(None),
            }
        }
        let Some(got_bin) = restore_tree(cas, &dir, bin_tree)? else {
            return Ok(None);
        };
        let Some(got_lib) = restore_tree(cas, &dir, lib_tree)? else {
            return Ok(None);
        };
        let Some(got_sysroot) = restore_tree(cas, &dir, sysroot_tree)? else {
            return Ok(None);
        };
        let Some(bundle) = restore_bundle(cas, &dir, bundle_digest)? else {
            return Ok(None);
        };
        debug_assert_eq!(got_bin, bin_tree);
        debug_assert_eq!(got_lib, lib_tree);
        debug_assert_eq!(got_sysroot, sysroot_tree);
        let host_triple = match fs::read_to_string(dir.join("host_triple")) {
            Ok(text) => text,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err),
        };
        let version_verbose = match fs::read_to_string(dir.join("version_verbose")) {
            Ok(text) => text,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err),
        };
        Ok(Some(CachedCapture {
            rustc_blob,
            sysroot_tree,
            bundle,
            host_triple,
            version_verbose,
        }))
    }

    /// Stores a freshly captured toolchain. Best-effort by contract: the
    /// caller logs and continues on failure (the cache is a performance
    /// optimization, never a correctness input).
    #[allow(clippy::too_many_arguments)]
    fn store(
        &self,
        cas: &Cas,
        key: &Digest,
        snapshot: &Digest,
        rustc_blob: BlobDigest,
        bin_tree: TreeDigest,
        lib_tree: TreeDigest,
        sysroot_tree: TreeDigest,
        bundle: &EnvironmentBundle,
        host_triple: &str,
        version_verbose: &str,
    ) -> io::Result<()> {
        let dir = self.entry_dir(key);
        fs::create_dir_all(dir.join("objects").join("blob"))?;
        fs::create_dir_all(dir.join("objects").join("tree"))?;
        fs::create_dir_all(dir.join("objects").join("bundle"))?;

        let rustc_bytes = cas.read_blob(rustc_blob)?;
        write_atomic(
            &dir.join("objects")
                .join("blob")
                .join(rustc_blob.digest().to_hex()),
            &rustc_bytes,
        )?;
        write_tree_object(&dir, cas, bin_tree)?;
        write_tree_object(&dir, cas, lib_tree)?;
        write_tree_object(&dir, cas, sysroot_tree)?;
        write_bundle_object(&dir, bundle)?;
        write_atomic(&dir.join("snapshot"), snapshot.to_hex().as_bytes())?;
        write_atomic(&dir.join("host_triple"), host_triple.as_bytes())?;
        write_atomic(&dir.join("version_verbose"), version_verbose.as_bytes())?;
        let manifest = format!(
            "rustc_blob {}\nbin_tree {}\nlib_tree {}\nsysroot_tree {}\nbundle {}\n",
            rustc_blob.digest().to_hex(),
            bin_tree.digest().to_hex(),
            lib_tree.digest().to_hex(),
            sysroot_tree.digest().to_hex(),
            bundle.digest().to_hex(),
        );
        // The manifest is the commit point: it is written last, so a
        // partial entry is never loadable.
        write_atomic(&dir.join("capture"), manifest.as_bytes())?;
        // Bounded cache: evict the oldest entries beyond the caps — never
        // the entry just committed. Best-effort; cache hygiene is a
        // performance concern, not a correctness one.
        self.prune(key);
        Ok(())
    }

    /// Deletes the oldest cache entries until the entry count and total
    /// size stay within [`MAX_ENTRIES`]/[`MAX_BYTES`]. The entry named
    /// `keep` (the one just stored) is never evicted. Failures only skip
    /// the optimization.
    fn prune(&self, keep: &Digest) {
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return;
        };
        let mut dirs: Vec<(PathBuf, std::time::SystemTime, u64)> = Vec::new();
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() {
                continue;
            }
            let path = entry.path();
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            let Ok(modified) = meta.modified() else {
                continue;
            };
            let Ok(size) = dir_size(&path) else {
                continue;
            };
            dirs.push((path, modified, size));
        }
        if dirs.len() <= MAX_ENTRIES && dirs.iter().map(|d| d.2).sum::<u64>() <= MAX_BYTES {
            return;
        }
        dirs.sort_by_key(|(_, modified, _)| *modified);
        let mut count = dirs.len();
        let mut bytes: u64 = dirs.iter().map(|d| d.2).sum();
        let keep_hex = keep.to_hex();
        for (path, _, size) in dirs {
            if count <= MAX_ENTRIES && bytes <= MAX_BYTES {
                break;
            }
            if path.file_name().and_then(|name| name.to_str()) == Some(&keep_hex) {
                continue;
            }
            if fs::remove_dir_all(&path).is_ok() {
                count -= 1;
                bytes -= size;
            }
        }
    }
}

/// Collects the paths (and symlink targets) of one sysroot subtree.
fn collect_snapshot_paths(
    dir: &Path,
    rel_prefix: &str,
    files: &mut Vec<(String, PathBuf)>,
    symlinks: &mut Vec<(String, String)>,
) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().into_string().map_err(|name| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("non-UTF-8 file name {name:?} in {}", dir.display()),
            )
        })?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        let rel = format!("{rel_prefix}/{name}");
        if file_type.is_dir() {
            collect_snapshot_paths(&path, &rel, files, symlinks)?;
        } else if file_type.is_symlink() {
            let target = fs::read_link(&path)?;
            symlinks.push((rel, target.to_string_lossy().into_owned()));
        } else if file_type.is_file() {
            files.push((rel, path));
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported file type: {}", path.display()),
            ));
        }
    }
    Ok(())
}

/// Stats every file concurrently, returning `(relpath, FileStat)`. Digests
/// are identical to a serial pass; only throughput changes.
fn stat_files_parallel(files: &[(String, PathBuf)]) -> io::Result<Vec<(String, FileStat)>> {
    let n = files.len();
    if n == 0 {
        return Ok(Vec::new());
    }
    let threads = std::thread::available_parallelism()
        .map(|p| p.get())
        .unwrap_or(4)
        .min(n);
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<(usize, io::Result<FileStat>)>> = Mutex::new(Vec::with_capacity(n));
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= n {
                        break;
                    }
                    let stat = fs::symlink_metadata(&files[i].1).and_then(|meta| {
                        let mtime_ns = meta
                            .modified()?
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_nanos() as u64)
                            .unwrap_or(0);
                        Ok((mtime_ns, meta.len(), file_mode(&meta)))
                    });
                    results.lock().unwrap().push((i, stat));
                }
            });
        }
    });
    let mut out = Vec::with_capacity(n);
    for (i, stat) in results.into_inner().unwrap() {
        out.push((files[i].0.clone(), stat?));
    }
    Ok(out)
}

#[cfg(unix)]
fn file_mode(meta: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o7777
}

#[cfg(not(unix))]
fn file_mode(_meta: &fs::Metadata) -> u32 {
    0
}

/// Recursive total size of a cache entry directory.
fn dir_size(dir: &Path) -> io::Result<u64> {
    let mut total = 0u64;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let meta = entry.metadata()?;
        if meta.is_dir() {
            total += dir_size(&entry.path())?;
        } else {
            total += meta.len();
        }
    }
    Ok(total)
}

/// Reads one cached object file; `Ok(None)` when absent.
fn read_cached(dir: &Path, kind: &str, digest: Digest) -> io::Result<Option<Vec<u8>>> {
    let path = dir.join("objects").join(kind).join(digest.to_hex());
    match fs::read(&path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

/// Restores one cached tree object (schema version + canonical encoding),
/// returning `Ok(None)` when missing, undecodable, or digest-mismatched.
fn restore_tree(cas: &Cas, dir: &Path, digest: TreeDigest) -> io::Result<Option<TreeDigest>> {
    if cas.get_tree(digest)?.is_some() {
        return Ok(Some(digest));
    }
    let Some(bytes) = read_cached(dir, "tree", digest.digest())? else {
        return Ok(None);
    };
    let mut dec = Decoder::new(&bytes);
    let version = match dec.read_u32() {
        Ok(version) => version,
        Err(_) => return Ok(None),
    };
    if version != TREE_SCHEMA_VERSION {
        return Ok(None);
    }
    let tree = match Tree::decode(&mut dec) {
        Ok(tree) => tree,
        Err(_) => return Ok(None),
    };
    match cas.put_tree(&tree) {
        Ok(got) if got == digest => Ok(Some(got)),
        _ => Ok(None),
    }
}

/// Restores one cached bundle object, returning `Ok(None)` when missing,
/// undecodable, or digest-mismatched.
fn restore_bundle(cas: &Cas, dir: &Path, digest: Digest) -> io::Result<Option<EnvironmentBundle>> {
    if let Some(bundle) = cas.get_bundle(digest)? {
        return Ok(Some(bundle));
    }
    let Some(bytes) = read_cached(dir, "bundle", digest)? else {
        return Ok(None);
    };
    let mut dec = Decoder::new(&bytes);
    let version = match dec.read_u32() {
        Ok(version) => version,
        Err(_) => return Ok(None),
    };
    if version != ENVIRONMENT_BUNDLE_SCHEMA_VERSION {
        return Ok(None);
    }
    let bundle = match EnvironmentBundle::decode(&mut dec) {
        Ok(bundle) => bundle,
        Err(_) => return Ok(None),
    };
    match cas.put_bundle(&bundle) {
        Ok(got) if got == digest => Ok(Some(bundle)),
        _ => Ok(None),
    }
}

/// Writes one tree object file from the store's canonical encoding.
fn write_tree_object(dir: &Path, cas: &Cas, digest: TreeDigest) -> io::Result<()> {
    let tree = cas.get_tree(digest)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("tree {} missing from store", digest.digest()),
        )
    })?;
    let mut enc = Encoder::new();
    enc.write_u32(TREE_SCHEMA_VERSION);
    tree.encode(&mut enc);
    write_atomic(
        &dir.join("objects")
            .join("tree")
            .join(digest.digest().to_hex()),
        &enc.into_bytes(),
    )
}

/// Writes one bundle object file from the store's canonical encoding.
fn write_bundle_object(dir: &Path, bundle: &EnvironmentBundle) -> io::Result<()> {
    let mut enc = Encoder::new();
    enc.write_u32(ENVIRONMENT_BUNDLE_SCHEMA_VERSION);
    bundle.encode(&mut enc);
    write_atomic(
        &dir.join("objects")
            .join("bundle")
            .join(bundle.digest().to_hex()),
        &enc.into_bytes(),
    )
}

/// Atomic write: temp file (unique per process) then rename.
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("object");
    let tmp = path.with_file_name(format!(
        ".{name}.tmp{}-{}",
        std::process::id(),
        TMP_COUNTER.fetch_add(1, Ordering::Relaxed),
    ));
    let mut file = fs::File::create(&tmp)?;
    file.write_all(bytes)?;
    fs::rename(&tmp, path)
}

/// Parses the capture manifest: five `role <hex-digest>` lines.
fn parse_manifest(text: &str) -> Option<(BlobDigest, TreeDigest, TreeDigest, TreeDigest, Digest)> {
    let mut fields = BTreeMap::<&str, &str>::new();
    for line in text.lines() {
        let (role, hex) = line.split_once(' ')?;
        fields.insert(role, hex);
    }
    Some((
        BlobDigest::new(Digest::from_hex(fields.get("rustc_blob")?).ok()?),
        TreeDigest::new(Digest::from_hex(fields.get("bin_tree")?).ok()?),
        TreeDigest::new(Digest::from_hex(fields.get("lib_tree")?).ok()?),
        TreeDigest::new(Digest::from_hex(fields.get("sysroot_tree")?).ok()?),
        Digest::from_hex(fields.get("bundle")?).ok()?,
    ))
}
