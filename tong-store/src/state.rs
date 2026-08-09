//! Build-state manifests: the GC root set and the "last successful graph".
//!
//! Every successful build records a [`BuildManifest`] — the full object
//! closure of the build: every action's digest (the `results/` cache keys),
//! input roots, toolchain bundles, and the materialized artifact mapping.
//! These manifests are the roots for reachability-based garbage collection
//! ([`crate::gc`]): an object that no manifest references is garbage.
//!
//! Layout: `<store>/state/projects/<project_hash_hex>/<ts>-<graph>.state`.
//! The `ts` prefix makes file names sort chronologically, so keeping the N
//! newest files per project is a trivial directory scan. Files are written
//! atomically (temp file + rename, same pattern as `action_cache.rs`).

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use tong_core::artifact::{BlobDigest, TreeDigest};
use tong_core::canonical::{self, CanonicalDecode, CanonicalEncode, DecodeError, Decoder, Encoder};
use tong_core::digest::{Digest, Hasher};

/// Schema version of the build-manifest encoding.
pub const BUILD_MANIFEST_SCHEMA_VERSION: u32 = 1;

/// Manifests kept per project; older ones are superseded state and are
/// deleted at write time ("rebuilding clears the old cache").
pub const MANIFESTS_KEPT_PER_PROJECT: usize = 3;

/// The recorded object closure of one successful build.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildManifest {
    /// Encoding schema version.
    pub schema_version: u32,
    /// SHA-256 of the canonical bytes of the canonicalized absolute
    /// workspace root; identifies the project in shared stores.
    pub project_hash: Digest,
    /// Wall-clock creation time (unix seconds), informational.
    pub created_at_unix_secs: u64,
    /// Digest of the canonical sorted `(logical_id, action_digest)` pairs.
    pub graph_digest: Digest,
    /// Build profiles used (usually one).
    pub profiles: Vec<String>,
    /// Union of all planned action input-root digests (⊇ workspace and
    /// dependency source trees).
    pub sources: Vec<Digest>,
    /// Union of environment-bundle digests.
    pub toolchains: Vec<Digest>,
    /// Every action of the build, in execution order.
    pub actions: Vec<RecordedAction>,
    /// Materialized final artifacts: artifact name → outputs tree.
    pub artifacts: Vec<(String, TreeDigest)>,
}

/// One recorded action of a build.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedAction {
    /// The action's semantic digest — the `results/` cache key.
    pub action_digest: Digest,
    /// Graph identity (diagnostics only).
    pub logical_id: String,
    /// Diagnostic mnemonic.
    pub mnemonic: String,
    /// Input root; re-execution needs these inputs.
    pub input_root: TreeDigest,
    /// Executable blob digest, when the executable is a store blob.
    pub executable: Option<BlobDigest>,
    /// Environment-bundle digest, when one was attached.
    pub env_bundle: Option<Digest>,
    /// Captured output tree.
    pub outputs: TreeDigest,
    /// Captured stdout.
    pub stdout: BlobDigest,
    /// Captured stderr.
    pub stderr: BlobDigest,
    /// Wall-clock execution time, informational.
    pub duration_millis: u64,
}

impl CanonicalEncode for BuildManifest {
    fn encode(&self, enc: &mut Encoder) {
        enc.write_u32(self.schema_version);
        self.project_hash.encode(enc);
        self.created_at_unix_secs.encode(enc);
        self.graph_digest.encode(enc);
        self.profiles.encode(enc);
        self.sources.encode(enc);
        self.toolchains.encode(enc);
        self.actions.encode(enc);
        enc.write_u64(self.artifacts.len() as u64);
        for (name, tree) in &self.artifacts {
            enc.write_str(name);
            tree.encode(enc);
        }
    }
}

impl CanonicalDecode for BuildManifest {
    fn decode(dec: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        let schema_version = u32::decode(dec)?;
        if schema_version != BUILD_MANIFEST_SCHEMA_VERSION {
            return Err(DecodeError::InvalidTag(schema_version));
        }
        let project_hash = Digest::decode(dec)?;
        let created_at_unix_secs = u64::decode(dec)?;
        let graph_digest = Digest::decode(dec)?;
        let profiles = Vec::<String>::decode(dec)?;
        let sources = Vec::<Digest>::decode(dec)?;
        let toolchains = Vec::<Digest>::decode(dec)?;
        let actions = Vec::<RecordedAction>::decode(dec)?;
        let artifact_count = dec.read_u64()?;
        let mut artifacts = Vec::with_capacity(artifact_count as usize);
        for _ in 0..artifact_count {
            let name = String::decode(dec)?;
            let tree = TreeDigest::new(Digest::decode(dec)?);
            artifacts.push((name, tree));
        }
        Ok(Self {
            schema_version,
            project_hash,
            created_at_unix_secs,
            graph_digest,
            profiles,
            sources,
            toolchains,
            actions,
            artifacts,
        })
    }
}

impl CanonicalEncode for RecordedAction {
    fn encode(&self, enc: &mut Encoder) {
        self.action_digest.encode(enc);
        self.logical_id.encode(enc);
        self.mnemonic.encode(enc);
        self.input_root.encode(enc);
        enc.write_option(&self.executable);
        enc.write_option(&self.env_bundle);
        self.outputs.encode(enc);
        self.stdout.encode(enc);
        self.stderr.encode(enc);
        self.duration_millis.encode(enc);
    }
}

impl CanonicalDecode for RecordedAction {
    fn decode(dec: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        Ok(Self {
            action_digest: Digest::decode(dec)?,
            logical_id: String::decode(dec)?,
            mnemonic: String::decode(dec)?,
            input_root: TreeDigest::new(Digest::decode(dec)?),
            executable: Option::<Digest>::decode(dec)?.map(BlobDigest::new),
            env_bundle: Option::<Digest>::decode(dec)?,
            outputs: TreeDigest::new(Digest::decode(dec)?),
            stdout: BlobDigest::new(Digest::decode(dec)?),
            stderr: BlobDigest::new(Digest::decode(dec)?),
            duration_millis: u64::decode(dec)?,
        })
    }
}

/// The per-project build-state directory.
///
/// `open` points at `<store>/state/projects`; each project's manifests live
/// under its own `<project_hash_hex>/` subdirectory.
#[derive(Clone, Debug)]
pub struct StateStore {
    root: PathBuf,
}

impl StateStore {
    /// Opens (creating if needed) the state store inside an existing store
    /// root.
    pub fn open(store_root: &Path) -> io::Result<Self> {
        let root = store_root.join("state").join("projects");
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    fn project_dir(&self, project_hash: &Digest) -> PathBuf {
        self.root.join(project_hash.to_hex())
    }

    /// Records a build manifest, keeping the [`MANIFESTS_KEPT_PER_PROJECT`]
    /// newest manifests per project (superseded state is deleted).
    pub fn write(&self, manifest: &BuildManifest) -> io::Result<()> {
        let dir = self.project_dir(&manifest.project_hash);
        fs::create_dir_all(&dir)?;
        let name = format!(
            "{:020}-{}.state",
            manifest.created_at_unix_secs,
            manifest.graph_digest.to_hex()
        );
        let path = dir.join(&name);
        if path.exists() {
            return Ok(());
        }
        let bytes = canonical::encode_vec(manifest);
        let tmp = self.root.join(format!("tmp-{}", std::process::id()));
        fs::write(&tmp, &bytes)?;
        match fs::rename(&tmp, &path) {
            Ok(()) => {}
            Err(err) if path.exists() => {
                let _ = fs::remove_file(&tmp);
                let _ = err;
            }
            Err(err) => return Err(err),
        }

        // Prune superseded manifests (oldest first; names sort by timestamp).
        let mut entries: Vec<(String, PathBuf)> = fs::read_dir(&dir)?
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".state"))
            .map(|entry| {
                (
                    entry.file_name().to_string_lossy().into_owned(),
                    entry.path(),
                )
            })
            .collect();
        entries.sort();
        while entries.len() > MANIFESTS_KEPT_PER_PROJECT {
            let (_, path) = entries.remove(0);
            let _ = fs::remove_file(path);
        }
        Ok(())
    }

    /// The newest manifest of a project, if any.
    pub fn latest(&self, project_hash: &Digest) -> Option<BuildManifest> {
        newest_in(&self.project_dir(project_hash))
    }

    /// The newest manifest of every project — the GC root set in shared
    /// mode. Older manifests are retained on disk (diagnostics), but only
    /// the latest successful graph of each project is a GC root, so
    /// rebuilding clears the superseded cache.
    pub fn all(&self) -> Vec<BuildManifest> {
        let mut out = Vec::new();
        let Ok(projects) = fs::read_dir(&self.root) else {
            return out;
        };
        for project in projects.flatten() {
            if !project.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            if let Some(manifest) = newest_in(&project.path()) {
                out.push(manifest);
            }
        }
        out
    }

    /// Removes a project's manifests (used by `tong clean` in shared mode).
    pub fn remove_project(&self, project_hash: &Digest) -> io::Result<()> {
        let dir = self.project_dir(project_hash);
        if dir.exists() {
            fs::remove_dir_all(&dir)?;
        }
        Ok(())
    }
}

fn newest_in(dir: &Path) -> Option<BuildManifest> {
    let mut entries: Vec<(String, PathBuf)> = fs::read_dir(dir)
        .ok()?
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(".state"))
        .map(|entry| {
            (
                entry.file_name().to_string_lossy().into_owned(),
                entry.path(),
            )
        })
        .collect();
    entries.sort();
    entries
        .pop()
        .and_then(|(_, path)| read_manifest(&path).ok())
}

fn read_manifest(path: &Path) -> io::Result<BuildManifest> {
    let bytes = fs::read(path)?;
    canonical::decode_all(&bytes)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err.to_string()))
}

/// Computes the project hash for a workspace root: SHA-256 of the canonical
/// bytes of the canonicalized absolute root.
pub fn project_hash(root: &Path) -> io::Result<Digest> {
    let canonical = fs::canonicalize(root)?;
    Ok(Hasher::digest(canonical.to_string_lossy().as_bytes()))
}

/// Computes the graph digest from canonical sorted `(logical_id,
/// action_digest)` pairs.
pub fn graph_digest(pairs: &BTreeMap<String, Digest>) -> Digest {
    let mut enc = Encoder::new();
    enc.write_u64(pairs.len() as u64);
    for (logical_id, digest) in pairs {
        enc.write_str(logical_id);
        digest.encode(&mut enc);
    }
    enc.digest()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tong_core::digest::Hasher;

    fn sample_manifest(project: &Digest, graph: &Digest, created: u64) -> BuildManifest {
        BuildManifest {
            schema_version: BUILD_MANIFEST_SCHEMA_VERSION,
            project_hash: *project,
            created_at_unix_secs: created,
            graph_digest: *graph,
            profiles: vec!["dev".to_owned()],
            sources: vec![Hasher::digest(b"source")],
            toolchains: vec![Hasher::digest(b"toolchain")],
            actions: vec![RecordedAction {
                action_digest: Hasher::digest(b"action"),
                logical_id: "rust:lib:app:rlib".to_owned(),
                mnemonic: "RustLibrary".to_owned(),
                input_root: TreeDigest::new(Hasher::digest(b"inputs")),
                executable: Some(BlobDigest::new(Hasher::digest(b"rustc"))),
                env_bundle: Some(Hasher::digest(b"bundle")),
                outputs: TreeDigest::new(Hasher::digest(b"outputs")),
                stdout: BlobDigest::new(Hasher::digest(b"stdout")),
                stderr: BlobDigest::new(Hasher::digest(b"stderr")),
                duration_millis: 7,
            }],
            artifacts: vec![(
                "app".to_owned(),
                TreeDigest::new(Hasher::digest(b"artifact")),
            )],
        }
    }

    #[test]
    fn manifest_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateStore::open(dir.path()).unwrap();
        let project = Hasher::digest(b"project");
        let manifest = sample_manifest(&project, &Hasher::digest(b"graph"), 100);
        state.write(&manifest).unwrap();
        let latest = state.latest(&project).unwrap();
        assert_eq!(latest, manifest);
        assert_eq!(state.all(), vec![manifest]);
    }

    #[test]
    fn keeps_only_newest_manifests_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateStore::open(dir.path()).unwrap();
        let project = Hasher::digest(b"project");
        for created in 0..5u64 {
            let manifest = sample_manifest(&project, &Hasher::digest(&[created as u8]), created);
            state.write(&manifest).unwrap();
        }
        // Disk retains only the newest MANIFESTS_KEPT_PER_PROJECT files.
        let dir = state.project_dir(&project);
        let files: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".state"))
            .collect();
        assert_eq!(files.len(), MANIFESTS_KEPT_PER_PROJECT);

        // The GC root set is only the latest graph of the project:
        // rebuilding clears the superseded cache.
        let all = state.all();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].created_at_unix_secs, 4);
        assert_eq!(state.latest(&project).unwrap().created_at_unix_secs, 4);
    }

    #[test]
    fn removes_project_manifests() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateStore::open(dir.path()).unwrap();
        let a = Hasher::digest(b"a");
        let b = Hasher::digest(b"b");
        state
            .write(&sample_manifest(&a, &Hasher::digest(b"ga"), 1))
            .unwrap();
        state
            .write(&sample_manifest(&b, &Hasher::digest(b"gb"), 1))
            .unwrap();
        state.remove_project(&a).unwrap();
        assert!(state.latest(&a).is_none());
        assert!(state.latest(&b).is_some());
    }

    #[test]
    fn project_hash_is_stable_and_absolute() {
        let dir = tempfile::tempdir().unwrap();
        let hash = project_hash(dir.path()).unwrap();
        assert_eq!(hash, project_hash(dir.path()).unwrap());
    }

    #[test]
    fn graph_digest_is_order_independent() {
        let mut a = BTreeMap::new();
        a.insert("x".to_owned(), Hasher::digest(b"1"));
        a.insert("y".to_owned(), Hasher::digest(b"2"));
        let mut b = BTreeMap::new();
        b.insert("y".to_owned(), Hasher::digest(b"2"));
        b.insert("x".to_owned(), Hasher::digest(b"1"));
        assert_eq!(graph_digest(&a), graph_digest(&b));
    }
}
