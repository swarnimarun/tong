//! The local content-addressed store.
//!
//! Layout (PLAN.md section 10):
//!
//! ```text
//! <root>/
//!   blobs/<hex[0..2]>/<hex[2..]>   file contents
//!   trees/<hex[0..2]>/<hex[2..]>   canonical tree encodings
//!   tmp/                           in-progress writes
//! ```
//!
//! Identity comes exclusively from digests. Writes go to a temporary file,
//! are digest-verified, then atomically renamed into place (section 10.1);
//! duplicate writers are tolerated because the final rename is idempotent.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use tong_core::artifact::{BlobDigest, TreeDigest};
use tong_core::canonical::{self, CanonicalDecode, CanonicalEncode};
use tong_core::digest::{Digest, Hasher};
use tong_core::paths::RelativePath;
use tong_core::tree::{Tree, TreeEntry};

/// Directory names never captured as source content (PLAN.md section 8.3:
/// exclude known output directories).
pub const CAPTURE_EXCLUDES: &[&str] = &[".tong", "target", ".git", ".jj"];

const SOURCE_SNAPSHOT_SCHEMA_VERSION: u32 = 1;

/// A local content-addressed store.
#[derive(Clone, Debug)]
pub struct Cas {
    root: PathBuf,
}

/// Build-scoped memoization for action-result closure validation.
///
/// CAS objects are immutable. Once a blob or complete tree closure has been
/// observed during one build, every later cache result referencing it can
/// reuse that proof instead of walking the same dependency output again.
#[derive(Debug, Default)]
pub struct ClosureVerifier {
    blobs: BTreeSet<BlobDigest>,
    trees: BTreeSet<TreeDigest>,
    visiting: BTreeSet<TreeDigest>,
}

impl Cas {
    /// Opens (creating if needed) the store at `root`.
    pub fn open(root: impl Into<PathBuf>) -> io::Result<Self> {
        let root = root.into();
        fs::create_dir_all(root.join("tmp"))?;
        fs::create_dir_all(root.join("blobs"))?;
        fs::create_dir_all(root.join("trees"))?;
        fs::create_dir_all(root.join("bundles"))?;
        Ok(Self { root })
    }

    /// Returns the store root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn object_path(&self, namespace: &str, digest: Digest) -> PathBuf {
        let hex = digest.to_hex();
        self.root.join(namespace).join(&hex[..2]).join(&hex[2..])
    }

    /// Returns the filesystem path of a blob, if present.
    pub fn blob_path(&self, digest: BlobDigest) -> Option<PathBuf> {
        let path = self.object_path("blobs", digest.digest());
        path.exists().then_some(path)
    }

    /// Returns whether a blob is present.
    pub fn has_blob(&self, digest: BlobDigest) -> bool {
        self.blob_path(digest).is_some()
    }

    /// Returns whether a blob is present, memoizing the result for this
    /// build when it is present.
    pub fn has_blob_cached(&self, digest: BlobDigest, verifier: &mut ClosureVerifier) -> bool {
        if verifier.blobs.contains(&digest) {
            return true;
        }
        if self.has_blob(digest) {
            verifier.blobs.insert(digest);
            true
        } else {
            false
        }
    }

    /// Reads a blob into memory.
    pub fn read_blob(&self, digest: BlobDigest) -> io::Result<Vec<u8>> {
        let path = self
            .blob_path(digest)
            .ok_or_else(|| not_found(format!("blob {}", digest.digest())))?;
        fs::read(path)
    }

    /// Writes a blob, returning its digest. Atomic and idempotent.
    pub fn put_blob(&self, data: &[u8]) -> io::Result<BlobDigest> {
        let digest = BlobDigest::new(Hasher::digest(data));
        self.write_object("blobs", digest.digest(), data.len() as u64, |w| {
            w.write_all(data)
        })?;
        Ok(digest)
    }

    /// Imports a file into the store, streaming the hash. The file mode's
    /// executable bit is recorded by the caller in the containing tree.
    pub fn put_file(&self, path: &Path) -> io::Result<BlobDigest> {
        // Hash while copying to a temp file, then rename to the digest path.
        let tmp = self.root.join("tmp").join(unique_name());
        let (digest, len) = {
            let mut reader = fs::File::open(path)?;
            let mut writer = fs::File::create(&tmp)?;
            let mut hasher = Hasher::new();
            let mut buf = [0u8; 64 * 1024];
            let mut total = 0u64;
            loop {
                let n = reader.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                total += n as u64;
                hasher.update(&buf[..n]);
                writer.write_all(&buf[..n])?;
            }
            (hasher.finish(), total)
        };
        let digest = BlobDigest::new(digest);
        // The bytes in `tmp` were hashed while they were copied. Re-reading
        // the temporary file here used to double all source-capture I/O.
        self.place_preverified("blobs", digest.digest(), &tmp, Some(len))?;
        Ok(digest)
    }

    fn write_object(
        &self,
        namespace: &str,
        digest: Digest,
        len: u64,
        write: impl FnOnce(&mut fs::File) -> io::Result<()>,
    ) -> io::Result<()> {
        let tmp = self.root.join("tmp").join(unique_name());
        {
            let mut file = fs::File::create(&tmp)?;
            write(&mut file)?;
        }
        self.place_verified(namespace, digest, &tmp, Some(len))
    }

    /// Verifies the temp file's digest and atomically moves it into place.
    fn place_verified(
        &self,
        namespace: &str,
        digest: Digest,
        tmp: &Path,
        expected_len: Option<u64>,
    ) -> io::Result<()> {
        let actual = hash_file(tmp)?;
        if actual != digest {
            let _ = fs::remove_file(tmp);
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("digest mismatch while storing: expected {digest}, wrote {actual}"),
            ));
        }
        self.place_preverified(namespace, digest, tmp, expected_len)
    }

    /// Publishes a temporary object whose bytes were already hashed by the
    /// caller while writing it.
    fn place_preverified(
        &self,
        namespace: &str,
        digest: Digest,
        tmp: &Path,
        expected_len: Option<u64>,
    ) -> io::Result<()> {
        if let Some(len) = expected_len {
            let actual_len = fs::metadata(tmp)?.len();
            if actual_len != len {
                let _ = fs::remove_file(tmp);
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("length mismatch while storing {digest}"),
                ));
            }
        }
        let dest = self.object_path(namespace, digest);
        if dest.exists() {
            let stale = expected_len.is_some_and(|len| {
                fs::metadata(&dest)
                    .map(|meta| meta.len() != len)
                    .unwrap_or(true)
            });
            if stale {
                // A same-digest object with the wrong size is store
                // corruption (e.g. a blob truncated through an aliased
                // hard link); replace it with the verified copy.
                let _ = fs::remove_file(&dest);
            } else {
                // Duplicate writer: content is identical by construction.
                let _ = fs::remove_file(tmp);
                return Ok(());
            }
        }
        fs::create_dir_all(dest.parent().unwrap())?;
        // Read-only: stored objects are immutable (section 10.1).
        let mut perms = fs::metadata(tmp)?.permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            perms.set_mode(0o444);
        }
        fs::set_permissions(tmp, perms)?;
        match fs::rename(tmp, &dest) {
            Ok(()) => Ok(()),
            Err(err) if dest.exists() => {
                // Lost a duplicate-writer race; content is identical.
                let _ = fs::remove_file(tmp);
                let _ = err;
                Ok(())
            }
            Err(err) => Err(err),
        }
    }

    /// Stores a tree (assuming subtrees are already stored) and returns its
    /// digest.
    ///
    /// The stored bytes are exactly the digest pre-image: schema version
    /// followed by the canonical tree encoding.
    pub fn put_tree(&self, tree: &Tree) -> io::Result<TreeDigest> {
        let digest = tree.digest();
        let mut enc = canonical::Encoder::new();
        enc.write_u32(tong_core::tree::TREE_SCHEMA_VERSION);
        tree.encode(&mut enc);
        let bytes = enc.into_bytes();
        let len = bytes.len() as u64;
        self.write_object("trees", digest.digest(), len, |w| w.write_all(&bytes))?;
        Ok(digest)
    }

    /// Reads a stored tree by digest.
    pub fn get_tree(&self, digest: TreeDigest) -> io::Result<Option<Tree>> {
        let path = self.object_path("trees", digest.digest());
        if !path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(&path)?;
        let mut dec = canonical::Decoder::new(&bytes);
        let version = dec
            .read_u32()
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err.to_string()))?;
        if version != tong_core::tree::TREE_SCHEMA_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported tree schema version {version}"),
            ));
        }
        let tree = Tree::decode(&mut dec)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err.to_string()))?;
        dec.expect_end()
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err.to_string()))?;
        Ok(Some(tree))
    }

    /// Returns whether a tree and its complete transitive blob/subtree
    /// closure are present. Action-cache hits use this before exposing a
    /// recorded result to downstream actions.
    pub fn has_tree_closure(&self, root: TreeDigest) -> io::Result<bool> {
        self.has_tree_closure_cached(root, &mut ClosureVerifier::default())
    }

    /// Returns whether a tree closure is present, reusing proofs accumulated
    /// earlier in the same build.
    pub fn has_tree_closure_cached(
        &self,
        root: TreeDigest,
        verifier: &mut ClosureVerifier,
    ) -> io::Result<bool> {
        if verifier.trees.contains(&root) {
            return Ok(true);
        }
        if !verifier.visiting.insert(root) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("tree closure contains a cycle at {}", root.digest()),
            ));
        }
        let Some(tree) = self.get_tree(root)? else {
            verifier.visiting.remove(&root);
            return Ok(false);
        };
        for entry in tree.entries().values() {
            let complete = match entry {
                TreeEntry::File { digest, .. } => self.has_blob_cached(*digest, verifier),
                TreeEntry::Directory(subtree) => {
                    self.has_tree_closure_cached(*subtree, verifier)?
                }
                TreeEntry::Symlink { .. } => true,
            };
            if !complete {
                verifier.visiting.remove(&root);
                return Ok(false);
            }
        }
        verifier.visiting.remove(&root);
        verifier.trees.insert(root);
        Ok(true)
    }

    /// Stores an environment bundle, returning its digest. The stored bytes
    /// are the digest pre-image: schema version plus canonical encoding.
    pub fn put_bundle(
        &self,
        bundle: &tong_core::bundle::EnvironmentBundle,
    ) -> io::Result<tong_core::digest::Digest> {
        let digest = bundle.digest();
        let mut enc = canonical::Encoder::new();
        enc.write_u32(tong_core::bundle::ENVIRONMENT_BUNDLE_SCHEMA_VERSION);
        bundle.encode(&mut enc);
        let bytes = enc.into_bytes();
        let len = bytes.len() as u64;
        self.write_object("bundles", digest, len, |w| w.write_all(&bytes))?;
        Ok(digest)
    }

    /// Reads a stored environment bundle by digest.
    pub fn get_bundle(
        &self,
        digest: Digest,
    ) -> io::Result<Option<tong_core::bundle::EnvironmentBundle>> {
        let path = self.object_path("bundles", digest);
        if !path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(&path)?;
        let mut dec = canonical::Decoder::new(&bytes);
        let version = dec
            .read_u32()
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err.to_string()))?;
        if version != tong_core::bundle::ENVIRONMENT_BUNDLE_SCHEMA_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported bundle schema version {version}"),
            ));
        }
        let bundle = decode_bundle(&mut dec)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err.to_string()))?;
        dec.expect_end()
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err.to_string()))?;
        Ok(Some(bundle))
    }

    /// Captures a directory as a canonical tree, importing all file contents.
    ///
    /// Directories named in [`CAPTURE_EXCLUDES`] are skipped (PLAN.md
    /// section 8.3: known output directories never enter source inputs).
    pub fn capture_dir(&self, path: &Path) -> io::Result<TreeDigest> {
        self.capture_dir_filtered(path, &CAPTURE_EXCLUDES.iter().copied().collect())
    }

    /// Captures a directory, skipping excluded entries at its root.
    /// Output-directory names remain valid source names below that root
    /// (for example, Rust's `src/target/` module directory).
    pub fn capture_dir_filtered(
        &self,
        path: &Path,
        excludes: &std::collections::BTreeSet<&str>,
    ) -> io::Result<TreeDigest> {
        let canonical = fs::canonicalize(path)?;
        let fingerprint = metadata_fingerprint(&canonical, excludes)?;
        let snapshot = self.snapshot_path(&canonical, excludes);
        if let Some(tree) = read_snapshot(&snapshot, fingerprint)?
            && self.has_tree_closure(tree)?
        {
            return Ok(tree);
        }

        // Check metadata on both sides of capture. A concurrently edited
        // tree is still captured content-correctly, but is never saved as a
        // reusable metadata snapshot.
        let tree = self.walk_dir(&canonical, excludes, true)?;
        let after = metadata_fingerprint(&canonical, excludes)?;
        if fingerprint == after {
            write_snapshot(&snapshot, fingerprint, tree)?;
        }
        Ok(tree)
    }

    fn snapshot_path(&self, path: &Path, excludes: &BTreeSet<&str>) -> PathBuf {
        let mut enc = canonical::Encoder::new();
        enc.write_u32(SOURCE_SNAPSHOT_SCHEMA_VERSION);
        enc.write_str(&path.to_string_lossy());
        enc.write_u64(excludes.len() as u64);
        for exclude in excludes {
            enc.write_str(exclude);
        }
        self.root
            .join("snapshots")
            .join(format!("{}.snapshot", enc.digest().to_hex()))
    }

    /// Computes a directory's tree digest without importing file contents
    /// (fingerprint-only). Used for system-captured toolchains, whose files
    /// stay in place (PLAN.md section 5).
    pub fn fingerprint_dir(
        &self,
        path: &Path,
        excludes: &std::collections::BTreeSet<&str>,
    ) -> io::Result<TreeDigest> {
        self.walk_dir(path, excludes, false)
    }

    fn walk_dir(
        &self,
        path: &Path,
        excludes: &BTreeSet<&str>,
        import_blobs: bool,
    ) -> io::Result<TreeDigest> {
        if import_blobs {
            self.walk_dir_serial(path, excludes, true, true)
        } else {
            // Fingerprint mode: hash every file under `path` concurrently,
            // then rebuild the tree from the digests. The tree structure
            // and digests are identical to the serial walk — only the
            // hashing is parallel (large toolchain sysroots dominate the
            // system toolchain capture otherwise).
            let mut files: Vec<PathBuf> = Vec::new();
            collect_files(path, excludes, &mut files, true)?;
            let digests = hash_files_parallel(&files)?;
            self.build_fingerprint_tree(path, excludes, &digests, true)
        }
    }

    fn walk_dir_serial(
        &self,
        path: &Path,
        excludes: &BTreeSet<&str>,
        import_blobs: bool,
        root: bool,
    ) -> io::Result<TreeDigest> {
        let mut entries = std::collections::BTreeMap::new();
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let name = entry.file_name().into_string().map_err(|name| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("non-UTF-8 file name {name:?} in {}", path.display()),
                )
            })?;
            if root && excludes.contains(name.as_str()) {
                continue;
            }
            let file_type = entry.file_type()?;
            let tree_entry = if file_type.is_dir() {
                TreeEntry::Directory(self.walk_dir_serial(
                    &entry.path(),
                    excludes,
                    import_blobs,
                    false,
                )?)
            } else if file_type.is_symlink() {
                let target = fs::read_link(entry.path())?;
                TreeEntry::Symlink {
                    target: target.to_string_lossy().into_owned(),
                }
            } else if file_type.is_file() {
                let digest = if import_blobs {
                    self.put_file(&entry.path())?
                } else {
                    BlobDigest::new(hash_file(&entry.path())?)
                };
                TreeEntry::File {
                    digest,
                    executable: is_executable(&entry.path())?,
                }
            } else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unsupported file type: {}", entry.path().display()),
                ));
            };
            entries.insert(name, tree_entry);
        }
        let tree = Tree::new(entries)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err.to_string()))?;
        self.put_tree(&tree)
    }

    /// Recursively collects the regular-file paths under `path` for
    /// parallel fingerprinting (symlinks need no hashing; their targets
    /// are recorded when the tree is rebuilt).
    fn build_fingerprint_tree(
        &self,
        path: &Path,
        excludes: &BTreeSet<&str>,
        digests: &HashMap<PathBuf, BlobDigest>,
        root: bool,
    ) -> io::Result<TreeDigest> {
        let mut entries = BTreeMap::new();
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let name = entry.file_name().into_string().map_err(|name| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("non-UTF-8 file name {name:?} in {}", path.display()),
                )
            })?;
            if root && excludes.contains(name.as_str()) {
                continue;
            }
            let file_type = entry.file_type()?;
            let tree_entry = if file_type.is_dir() {
                TreeEntry::Directory(self.build_fingerprint_tree(
                    &entry.path(),
                    excludes,
                    digests,
                    false,
                )?)
            } else if file_type.is_symlink() {
                let target = fs::read_link(entry.path())?;
                TreeEntry::Symlink {
                    target: target.to_string_lossy().into_owned(),
                }
            } else if file_type.is_file() {
                let digest = digests.get(&entry.path()).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("fingerprint missing for {}", entry.path().display()),
                    )
                })?;
                TreeEntry::File {
                    digest: *digest,
                    executable: is_executable(&entry.path())?,
                }
            } else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unsupported file type: {}", entry.path().display()),
                ));
            };
            entries.insert(name, tree_entry);
        }
        let tree = Tree::new(entries)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err.to_string()))?;
        self.put_tree(&tree)
    }

    /// Materializes a tree into `dest` (created if missing), hardlinking
    /// blobs where possible.
    pub fn materialize(&self, tree: TreeDigest, dest: &Path) -> io::Result<()> {
        fs::create_dir_all(dest)?;
        let tree = self
            .get_tree(tree)?
            .ok_or_else(|| not_found(format!("tree {}", tree.digest())))?;
        for (name, entry) in tree.entries() {
            let path = dest.join(name);
            match entry {
                TreeEntry::File { digest, executable } => {
                    let blob = self
                        .blob_path(*digest)
                        .ok_or_else(|| not_found(format!("blob {}", digest.digest())))?;
                    if path.exists() {
                        fs::remove_file(&path)?;
                    }
                    // Copy, never hard-link: materialized files must be
                    // writable-removable and carry the tree's executable bit,
                    // and a chmod (or any later write) through a hard link
                    // would mutate the immutable store object. Copies keep
                    // store blobs read-only and corruption-proof.
                    fs::copy(&blob, &path)?;
                    let mut perms = fs::metadata(&path)?.permissions();
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        perms.set_mode(if *executable { 0o755 } else { 0o644 });
                    }
                    fs::set_permissions(&path, perms)?;
                }
                TreeEntry::Symlink { target } => {
                    if path.symlink_metadata().is_ok() {
                        fs::remove_file(&path)?;
                    }
                    #[cfg(unix)]
                    std::os::unix::fs::symlink(target, &path)?;
                    #[cfg(not(unix))]
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "symlink materialization is only implemented on unix",
                    ));
                }
                TreeEntry::Directory(sub) => self.materialize(*sub, &path)?,
            }
        }
        Ok(())
    }

    /// Assembles a single tree from mounts: `(mount point, tree)` pairs.
    ///
    /// The mount point `.` merges the tree at the root. Mounts under
    /// directories create intermediate directories as needed; conflicting
    /// non-directory entries are an error.
    pub fn assemble(&self, mounts: &[(RelativePath, TreeDigest)]) -> io::Result<TreeDigest> {
        // Trees are persistent values. Overlay only the directory nodes on
        // mount paths and at actual directory conflicts; expanding the
        // complete source tree for every action made warm cache checks do
        // filesystem work proportional to all source files, even when a
        // mount merely added `deps/foo` beside them.
        fn load(cas: &Cas, digest: TreeDigest) -> io::Result<Tree> {
            cas.get_tree(digest)?
                .ok_or_else(|| not_found(format!("tree {}", digest.digest())))
        }

        fn merge(cas: &Cas, base: TreeDigest, overlay: TreeDigest) -> io::Result<TreeDigest> {
            if base == overlay {
                return Ok(base);
            }
            let mut entries = load(cas, base)?.entries().clone();
            for (name, incoming) in load(cas, overlay)?.entries() {
                let next = match (entries.get(name), incoming) {
                    (Some(TreeEntry::Directory(left)), TreeEntry::Directory(right)) => {
                        TreeEntry::Directory(merge(cas, *left, *right)?)
                    }
                    (Some(TreeEntry::Directory(_)), _) | (Some(_), TreeEntry::Directory(_)) => {
                        return Err(conflict(name));
                    }
                    // As in the old assembler, a later non-directory mount
                    // replaces an earlier non-directory entry.
                    (_, entry) => entry.clone(),
                };
                entries.insert(name.clone(), next);
            }
            cas.put_tree(
                &Tree::new(entries)
                    .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err.to_string()))?,
            )
        }

        #[derive(Default)]
        struct MountNode {
            roots: Vec<TreeDigest>,
            children: BTreeMap<String, MountNode>,
        }

        fn add(node: &mut MountNode, components: &[&str], mounted: TreeDigest) {
            if let Some((name, rest)) = components.split_first() {
                add(
                    node.children.entry((*name).to_owned()).or_default(),
                    rest,
                    mounted,
                );
            } else {
                node.roots.push(mounted);
            }
        }

        fn store(cas: &Cas, node: &MountNode) -> io::Result<TreeDigest> {
            let mut base = None;
            for root in &node.roots {
                base = Some(match base {
                    Some(base) => merge(cas, base, *root)?,
                    None => *root,
                });
            }
            if node.children.is_empty() {
                return base.ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "empty mount node")
                });
            }
            let mut entries = match base {
                Some(base) => load(cas, base)?.entries().clone(),
                None => BTreeMap::new(),
            };
            for (name, child_node) in &node.children {
                let child = store(cas, child_node)?;
                let child = match entries.get(name) {
                    Some(TreeEntry::Directory(existing)) => merge(cas, *existing, child)?,
                    Some(_) => return Err(conflict(name)),
                    None => child,
                };
                entries.insert(name.clone(), TreeEntry::Directory(child));
            }
            cas.put_tree(
                &Tree::new(entries)
                    .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err.to_string()))?,
            )
        }

        let mut root = MountNode::default();
        for (mount, tree) in mounts {
            let components: Vec<&str> = if mount.as_str() == RelativePath::ROOT {
                Vec::new()
            } else {
                mount.as_str().split('/').collect()
            };
            add(&mut root, &components, *tree);
        }
        store(self, &root)
    }
}

fn decode_bundle(
    dec: &mut canonical::Decoder<'_>,
) -> Result<tong_core::bundle::EnvironmentBundle, canonical::DecodeError> {
    use tong_core::action::CanonicalValue;
    use tong_core::bundle::EnvironmentBundle;
    use tong_core::platform::PlatformKey;
    Ok(EnvironmentBundle {
        name: String::decode(dec)?,
        provider: String::decode(dec)?,
        platform: PlatformKey::new(std::collections::BTreeMap::decode(dec)?),
        variables: std::collections::BTreeMap::decode(dec)?,
        files: TreeDigest::new(Digest::decode(dec)?),
        metadata: std::collections::BTreeMap::<String, CanonicalValue>::decode(dec)?,
    })
}

fn hash_file(path: &Path) -> io::Result<Digest> {
    let mut reader = fs::File::open(path)?;
    let mut hasher = Hasher::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finish())
}

/// Hashes a directory's identity-bearing metadata without reading file
/// contents. On Unix, ctime plus device/inode catches preserved-mtime edits
/// and file replacement; names, types, modes, and symlink targets are also
/// part of the fingerprint.
fn metadata_fingerprint(path: &Path, excludes: &BTreeSet<&str>) -> io::Result<Digest> {
    fn encode_metadata(
        enc: &mut canonical::Encoder,
        relative: &Path,
        metadata: &fs::Metadata,
        kind: u32,
    ) {
        enc.write_str(&relative.to_string_lossy());
        enc.write_u32(kind);
        enc.write_u64(metadata.len());
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            enc.write_i64(metadata.mtime());
            enc.write_i64(metadata.mtime_nsec());
            enc.write_i64(metadata.ctime());
            enc.write_i64(metadata.ctime_nsec());
            enc.write_u64(metadata.dev());
            enc.write_u64(metadata.ino());
            enc.write_u32(metadata.mode());
        }
        #[cfg(not(unix))]
        {
            let modified = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .unwrap_or_default();
            enc.write_u64(modified.as_secs());
            enc.write_u32(modified.subsec_nanos());
            enc.write_bool(metadata.permissions().readonly());
        }
    }

    fn walk(
        enc: &mut canonical::Encoder,
        root: &Path,
        relative: &Path,
        excludes: &BTreeSet<&str>,
    ) -> io::Result<()> {
        let dir = root.join(relative);
        encode_metadata(enc, relative, &fs::symlink_metadata(&dir)?, 0);
        let mut entries: Vec<fs::DirEntry> = fs::read_dir(&dir)?.collect::<Result<_, _>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let name = entry.file_name().into_string().map_err(|name| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("non-UTF-8 file name {name:?} in {}", dir.display()),
                )
            })?;
            if relative.as_os_str().is_empty() && excludes.contains(name.as_str()) {
                continue;
            }
            let child = relative.join(&name);
            let metadata = fs::symlink_metadata(entry.path())?;
            let file_type = metadata.file_type();
            if file_type.is_dir() {
                walk(enc, root, &child, excludes)?;
            } else if file_type.is_file() {
                encode_metadata(enc, &child, &metadata, 1);
            } else if file_type.is_symlink() {
                encode_metadata(enc, &child, &metadata, 2);
                enc.write_str(&fs::read_link(entry.path())?.to_string_lossy());
            } else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unsupported file type: {}", entry.path().display()),
                ));
            }
        }
        Ok(())
    }

    let mut enc = canonical::Encoder::new();
    enc.write_u32(SOURCE_SNAPSHOT_SCHEMA_VERSION);
    walk(&mut enc, path, Path::new(""), excludes)?;
    Ok(enc.digest())
}

fn read_snapshot(path: &Path, fingerprint: Digest) -> io::Result<Option<TreeDigest>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    let mut dec = canonical::Decoder::new(&bytes);
    let decoded = (|| {
        let version = dec.read_u32().ok()?;
        if version != SOURCE_SNAPSHOT_SCHEMA_VERSION {
            return None;
        }
        let recorded = Digest::decode(&mut dec).ok()?;
        let tree = TreeDigest::new(Digest::decode(&mut dec).ok()?);
        dec.expect_end().ok()?;
        (recorded == fingerprint).then_some(tree)
    })();
    Ok(decoded)
}

fn write_snapshot(path: &Path, fingerprint: Digest, tree: TreeDigest) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "snapshot has no parent"))?;
    fs::create_dir_all(parent)?;
    let mut enc = canonical::Encoder::new();
    enc.write_u32(SOURCE_SNAPSHOT_SCHEMA_VERSION);
    fingerprint.encode(&mut enc);
    tree.encode(&mut enc);
    let tmp = parent.join(format!("tmp-{}", unique_name()));
    fs::write(&tmp, enc.into_bytes())?;
    match fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(err) if path.is_file() => {
            // Another build published an equally valid acceleration entry.
            let _ = fs::remove_file(tmp);
            let _ = err;
            Ok(())
        }
        Err(err) => Err(err),
    }
}

/// Recursively collects the regular-file paths under `path` for parallel
/// fingerprinting. Symlinks and unsupported file types are skipped here;
/// the tree rebuild records them.
fn collect_files(
    path: &Path,
    excludes: &BTreeSet<&str>,
    files: &mut Vec<PathBuf>,
    root: bool,
) -> io::Result<()> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let name = entry.file_name().into_string().map_err(|name| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("non-UTF-8 file name {name:?} in {}", path.display()),
            )
        })?;
        if root && excludes.contains(name.as_str()) {
            continue;
        }
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_files(&entry.path(), excludes, files, false)?;
        } else if file_type.is_file() {
            files.push(entry.path());
        }
    }
    Ok(())
}

/// Hashes every file concurrently, returning a path → digest map. Digests
/// are identical to a serial pass; only the throughput changes.
fn hash_files_parallel(files: &[PathBuf]) -> io::Result<HashMap<PathBuf, BlobDigest>> {
    let n = files.len();
    if n == 0 {
        return Ok(HashMap::new());
    }
    let threads = std::thread::available_parallelism()
        .map(|p| p.get())
        .unwrap_or(4)
        .min(n);
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<(usize, io::Result<BlobDigest>)>> = Mutex::new(Vec::with_capacity(n));
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= n {
                        break;
                    }
                    let digest = hash_file(&files[i]).map(BlobDigest::new);
                    results.lock().unwrap().push((i, digest));
                }
            });
        }
    });
    let mut digests = HashMap::with_capacity(n);
    for (i, result) in results.into_inner().unwrap() {
        digests.insert(files[i].clone(), result?);
    }
    Ok(digests)
}

fn is_executable(path: &Path) -> io::Result<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Ok(fs::metadata(path)?.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(false)
    }
}

fn unique_name() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!(
        "{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

fn not_found(what: impl Into<String>) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        format!("{} not in store", what.into()),
    )
}

fn conflict(name: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("conflicting entries for {name:?} while assembling tree"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_cas() -> (tempfile::TempDir, Cas) {
        let dir = tempfile::tempdir().unwrap();
        let cas = Cas::open(dir.path().join("store")).unwrap();
        (dir, cas)
    }

    #[test]
    fn blob_roundtrip() {
        let (_dir, cas) = temp_cas();
        let digest = cas.put_blob(b"hello tong").unwrap();
        assert!(cas.has_blob(digest));
        assert_eq!(cas.read_blob(digest).unwrap(), b"hello tong");
    }

    #[test]
    fn capture_and_materialize_roundtrip() {
        let (_dir, cas) = temp_cas();
        let src = cas.root().parent().unwrap().join("src");
        fs::create_dir_all(src.join("sub")).unwrap();
        fs::write(src.join("a.txt"), b"aaa").unwrap();
        fs::write(src.join("sub/b.txt"), b"bbb").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("a.txt", src.join("link")).unwrap();

        let tree = cas.capture_dir(&src).unwrap();
        let out = cas.root().parent().unwrap().join("out");
        cas.materialize(tree, &out).unwrap();

        assert_eq!(fs::read(out.join("a.txt")).unwrap(), b"aaa");
        assert_eq!(fs::read(out.join("sub/b.txt")).unwrap(), b"bbb");
        #[cfg(unix)]
        assert_eq!(
            fs::read_link(out.join("link")).unwrap().to_str().unwrap(),
            "a.txt"
        );
    }

    #[cfg(unix)]
    #[test]
    fn source_snapshot_detects_preserved_mtime_edit() {
        let (_dir, cas) = temp_cas();
        let src = cas.root().parent().unwrap().join("snapshot-src");
        fs::create_dir_all(&src).unwrap();
        let file = src.join("same-size.txt");
        let timestamp = src.join("timestamp");
        fs::write(&file, b"before").unwrap();
        fs::copy(&file, &timestamp).unwrap();
        assert!(
            std::process::Command::new("touch")
                .args(["-r"])
                .arg(&file)
                .arg(&timestamp)
                .status()
                .unwrap()
                .success()
        );

        let before = cas.capture_dir(&src).unwrap();
        assert_eq!(cas.capture_dir(&src).unwrap(), before);

        fs::write(&file, b"after!").unwrap();
        assert!(
            std::process::Command::new("touch")
                .args(["-r"])
                .arg(&timestamp)
                .arg(&file)
                .status()
                .unwrap()
                .success()
        );
        let after = cas.capture_dir(&src).unwrap();
        assert_ne!(
            after, before,
            "ctime must invalidate a preserved-mtime edit"
        );
    }

    #[test]
    fn capture_excludes_output_dirs() {
        let (_dir, cas) = temp_cas();
        let src = cas.root().parent().unwrap().join("src2");
        fs::create_dir_all(src.join("target")).unwrap();
        fs::create_dir_all(src.join(".tong")).unwrap();
        fs::create_dir_all(src.join("src/target")).unwrap();
        fs::write(src.join("keep.txt"), b"k").unwrap();
        fs::write(src.join("target/junk"), b"j").unwrap();
        fs::write(src.join("src/target/module.rs"), b"source").unwrap();

        let tree = cas.capture_dir(&src).unwrap();
        let tree = cas.get_tree(tree).unwrap().unwrap();
        assert!(tree.entries().contains_key("keep.txt"));
        assert!(!tree.entries().contains_key("target"));
        assert!(!tree.entries().contains_key(".tong"));
        let nested = match tree.entries().get("src").unwrap() {
            TreeEntry::Directory(digest) => cas.get_tree(*digest).unwrap().unwrap(),
            other => panic!("src should be a directory, got {other:?}"),
        };
        assert!(nested.entries().contains_key("target"));
    }

    #[test]
    fn assemble_merges_mounts() {
        let (_dir, cas) = temp_cas();
        let base_dir = cas.root().parent().unwrap().join("base");
        let dep_dir = cas.root().parent().unwrap().join("dep");
        fs::create_dir_all(&base_dir).unwrap();
        fs::create_dir_all(&dep_dir).unwrap();
        fs::write(base_dir.join("main.rs"), b"fn main() {}").unwrap();
        fs::write(dep_dir.join("libfoo.rlib"), b"rlib").unwrap();

        let base = cas.capture_dir(&base_dir).unwrap();
        let dep = cas.capture_dir(&dep_dir).unwrap();
        let merged = cas
            .assemble(&[
                (RelativePath::new(".").unwrap(), base),
                (RelativePath::new("deps").unwrap(), dep),
            ])
            .unwrap();

        let out = cas.root().parent().unwrap().join("merged");
        cas.materialize(merged, &out).unwrap();
        assert_eq!(fs::read(out.join("main.rs")).unwrap(), b"fn main() {}");
        assert_eq!(fs::read(out.join("deps/libfoo.rlib")).unwrap(), b"rlib");
    }

    #[test]
    fn stored_objects_are_read_only() {
        let (_dir, cas) = temp_cas();
        let digest = cas.put_blob(b"immutable").unwrap();
        let path = cas.blob_path(digest).unwrap();
        let perms = fs::metadata(&path).unwrap().permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(perms.mode() & 0o222, 0);
        }
        let _ = perms;
    }

    #[test]
    fn materialized_files_do_not_alias_blobs() {
        // Regression: materialize used to hard-link blobs and then chmod the
        // link, which mutated the shared inode — the store blob lost its
        // read-only mode and any later write through the link truncated it.
        let (_dir, cas) = temp_cas();
        let digest = cas.put_blob(b"payload").unwrap();
        let tree = Tree::new(
            [(
                "f".to_owned(),
                TreeEntry::File {
                    digest,
                    executable: true,
                },
            )]
            .into_iter()
            .collect(),
        )
        .unwrap();
        let tree = cas.put_tree(&tree).unwrap();

        let out1 = cas.root().parent().unwrap().join("out1");
        cas.materialize(tree, &out1).unwrap();

        // Writing to the materialized copy must not corrupt the blob.
        fs::write(out1.join("f"), b"tampered").unwrap();
        assert_eq!(cas.read_blob(digest).unwrap(), b"payload");

        // Re-materializing (same tree, different exec expectations)
        // keeps the blob intact and honors the bit on the copy.
        let out2 = cas.root().parent().unwrap().join("out2");
        cas.materialize(tree, &out2).unwrap();
        assert_eq!(fs::read(out2.join("f")).unwrap(), b"payload");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_ne!(
                fs::metadata(out2.join("f")).unwrap().permissions().mode() & 0o111,
                0,
                "executable bit must be set on the copy"
            );
            // The store blob keeps its read-only mode.
            assert_eq!(
                fs::metadata(cas.blob_path(digest).unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o222,
                0
            );
        }
    }

    #[test]
    fn put_file_replaces_truncated_blob() {
        // Regression: a blob truncated in the store used to be silently
        // reused (idempotent skip), propagating the corruption. A same-
        // digest object with the wrong length must be replaced.
        let (_dir, cas) = temp_cas();
        let src = cas.root().parent().unwrap().join("src.bin");
        fs::write(&src, b"real content").unwrap();
        let digest = cas.put_file(&src).unwrap();

        let path = cas.blob_path(digest).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        }
        fs::write(&path, b"").unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), 0);

        let again = cas.put_file(&src).unwrap();
        assert_eq!(again, digest);
        assert_eq!(cas.read_blob(digest).unwrap(), b"real content");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o222,
                0,
                "replaced blob must be read-only again"
            );
        }
    }
}
