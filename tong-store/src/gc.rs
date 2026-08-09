//! Reachability-based garbage collection over the whole store.
//!
//! The GC root set is the build-state manifests ([`crate::state`]): every
//! digest they reference — action digests (`results/` entries), input-root
//! trees, source blobs, toolchain bundles — is marked; everything else is
//! garbage. Mark-and-sweep mirrors Cargo's GC direction (#5026/#16804) but
//! uses CAS reachability instead of SQLite mtime tracking.
//!
//! Two deletion policies compose:
//!
//! - **Age**: unmarked objects older than `older_than` are deleted
//!   (default retention; `tong gc --older-than 0` deletes all unmarked).
//! - **Budget**: when the store exceeds `max_size`, unmarked objects are
//!   deleted oldest-first until under budget — but never objects younger
//!   than 24h, which protects concurrent in-flight builds in shared mode
//!   whose manifests are not written yet.
//!
//! Results are swept before objects, so no result ever references a swept
//! blob. Stale `<store>/tmp/*` files (atomic-write leftovers) older than
//! 24h are always deleted.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tong_core::artifact::TreeDigest;
use tong_core::digest::{DIGEST_HEX_LEN, Digest};
use tong_core::tree::TreeEntry;

use crate::cas::Cas;
use crate::state::StateStore;

/// Objects younger than this are never deleted by a budget sweep (protects
/// concurrent in-flight builds whose manifests aren't written yet).
const BUDGET_AGE_FLOOR: Duration = Duration::from_secs(24 * 3600);

/// Stale tmp files older than this are always deleted.
const TMP_AGE: Duration = Duration::from_secs(24 * 3600);

/// Object namespaces, in sweep order: results first, then objects, so no
/// result references a swept blob.
const NAMESPACES: &[&str] = &["results", "blobs", "trees", "bundles", "toolchains"];

/// GC options.
#[derive(Clone, Debug)]
pub struct GcOptions {
    /// Age floor for deleting unmarked objects. `None` (or `Some(ZERO)`)
    /// deletes all unmarked objects regardless of age.
    pub older_than: Option<Duration>,
    /// Store size budget: unmarked objects are deleted oldest-first until
    /// the store fits, never objects younger than 24h.
    pub max_size: Option<u64>,
    /// Compute and report without deleting.
    pub dry_run: bool,
    /// Reference time (unix seconds) for age comparisons; tests inject it.
    pub now: u64,
}

impl Default for GcOptions {
    fn default() -> Self {
        Self {
            older_than: None,
            max_size: None,
            dry_run: false,
            now: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        }
    }
}

/// GC outcome counts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GcReport {
    /// Marked `results/` entries (live action results).
    pub marked_results: usize,
    /// Marked objects (blobs, trees, bundles, toolchains).
    pub marked_objects: usize,
    /// Deleted `results/` entries.
    pub deleted_results: usize,
    /// Deleted objects (blobs, trees, bundles, toolchains, stale tmp).
    pub deleted_objects: usize,
    /// Total store size before the sweep, in bytes.
    pub store_bytes: u64,
    /// Bytes freed by the sweep.
    pub freed_bytes: u64,
}

impl std::fmt::Display for GcReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "gc: kept {} results, {} objects; deleted {} results, {} objects \
             ({} freed, store {} bytes)",
            self.marked_results,
            self.marked_objects,
            self.deleted_results,
            self.deleted_objects,
            self.freed_bytes,
            self.store_bytes,
        )
    }
}

/// A candidate for deletion: an unmarked object with its mtime and size.
struct Candidate {
    mtime_secs: u64,
    size: u64,
    path: PathBuf,
}

/// Marks and sweeps the store rooted at `cas` against `state`'s manifests.
pub fn sweep(cas: &Cas, state: &StateStore, opts: &GcOptions) -> io::Result<GcReport> {
    let root = cas.root();

    // --- Mark -------------------------------------------------------------
    // Reachability, not just direct digests: trees reference blobs and
    // subtrees, bundles reference trees. A marked tree whose blobs were
    // swept would corrupt the cache, so the mark walks the closure.
    let mut digests: BTreeSet<Digest> = BTreeSet::new();
    for manifest in state.all() {
        for action in &manifest.actions {
            digests.insert(action.action_digest);
            digests.insert(action.input_root.digest());
            if let Some(executable) = action.executable {
                digests.insert(executable.digest());
            }
            if let Some(bundle) = action.env_bundle {
                digests.insert(bundle);
            }
            digests.insert(action.outputs.digest());
            digests.insert(action.stdout.digest());
            digests.insert(action.stderr.digest());
        }
        for digest in &manifest.sources {
            digests.insert(*digest);
        }
        for digest in &manifest.toolchains {
            digests.insert(*digest);
        }
        for (_, tree) in &manifest.artifacts {
            digests.insert(tree.digest());
        }
    }

    // Close the reachability graph: trees → blobs/subtrees, bundles →
    // trees. Repeatedly scan until fixpoint (each pass adds at least one
    // digest or stops).
    let mut scanned: BTreeSet<Digest> = BTreeSet::new();
    loop {
        let mut added = false;
        let mut pending: Vec<Digest> = digests.difference(&scanned).copied().collect();
        pending.sort();
        for digest in pending {
            scanned.insert(digest);
            if let Some(tree) = cas.get_tree(TreeDigest::new(digest))? {
                for entry in tree.entries().values() {
                    match entry {
                        TreeEntry::File { digest, .. } => {
                            added |= digests.insert(digest.digest());
                        }
                        TreeEntry::Directory(sub) => {
                            added |= digests.insert(sub.digest());
                        }
                        TreeEntry::Symlink { .. } => {}
                    }
                }
            }
            if let Some(bundle) = cas.get_bundle(digest)? {
                added |= digests.insert(bundle.files.digest());
            }
        }
        if !added {
            break;
        }
    }

    // --- Collect candidates and sizes ------------------------------------
    let mut report = GcReport::default();
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut tmp_candidates: Vec<Candidate> = Vec::new();

    for namespace in NAMESPACES {
        let dir = root.join(namespace);
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err),
        };
        for entry in entries.flatten() {
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                collect_sharded(&dir.join(entry.file_name()), namespace, &digests, &mut report, &mut candidates)?;
            } else if file_type.is_file() {
                // A stray file (e.g. `ActionCache::put`'s `tmp-<pid>`
                // leftover): unmarked by construction.
                collect_file(&entry.path(), &mut report, &mut candidates)?;
            }
        }
    }

    // Stale atomic-write leftovers in <store>/tmp/.
    if let Ok(entries) = fs::read_dir(root.join("tmp")) {
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|t| t.is_file()) {
                collect_file(&entry.path(), &mut report, &mut tmp_candidates)?;
            }
        }
    }

    let mut deleted: BTreeSet<PathBuf> = BTreeSet::new();
    let delete = |candidate: &Candidate, report: &mut GcReport, deleted: &mut BTreeSet<PathBuf>| -> io::Result<()> {
        if !opts.dry_run {
            fs::remove_file(&candidate.path)?;
        }
        report.freed_bytes += candidate.size;
        deleted.insert(candidate.path.clone());
        Ok(())
    };

    // --- Sweep by age: results first, then objects ------------------------
    // Candidates are already grouped results-then-objects in NAMESPACES
    // order (tmp files were collected separately, after objects).
    for candidate in &candidates {
        if !is_older_than(candidate.mtime_secs, opts.now, opts.older_than) {
            continue;
        }
        let is_result = candidate.path.starts_with(root.join("results"));
        if is_result {
            report.deleted_results += 1;
        } else {
            report.deleted_objects += 1;
        }
        delete(candidate, &mut report, &mut deleted)?;
    }
    for candidate in &tmp_candidates {
        if candidate.mtime_secs + TMP_AGE.as_secs() > opts.now {
            continue;
        }
        report.deleted_objects += 1;
        delete(candidate, &mut report, &mut deleted)?;
    }

    // --- Sweep by budget ---------------------------------------------------
    if let Some(budget) = opts.max_size
        && report.store_bytes > budget
    {
        let mut remaining: Vec<&Candidate> = candidates
            .iter()
            .filter(|candidate| !deleted.contains(&candidate.path))
            .filter(|candidate| candidate.mtime_secs + BUDGET_AGE_FLOOR.as_secs() <= opts.now)
            .collect();
        remaining.sort_by_key(|candidate| candidate.mtime_secs);
        let mut current = report.store_bytes - report.freed_bytes;
        for candidate in remaining {
            if current <= budget {
                break;
            }
            let is_result = candidate.path.starts_with(root.join("results"));
            if is_result {
                report.deleted_results += 1;
            } else {
                report.deleted_objects += 1;
            }
            current -= candidate.size;
            delete(candidate, &mut report, &mut deleted)?;
        }
    }

    Ok(report)
}

/// Walks a sharded namespace subdirectory (`<ns>/<hex2>/<hex62>`), marking
/// digest-addressed files and collecting unmarked candidates.
fn collect_sharded(
    dir: &Path,
    namespace: &str,
    digests: &BTreeSet<Digest>,
    report: &mut GcReport,
    candidates: &mut Vec<Candidate>,
) -> io::Result<()> {
    let entries = fs::read_dir(dir)?;
    for entry in entries.flatten() {
        let file_type = entry.file_type()?;
        if !file_type.is_file() {
            continue;
        }
        let shard = dir
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        let Some(digest) = digest_from_name(shard, entry.file_name().to_str().unwrap_or("")) else {
            // Not a digest-addressed file (leftover temp): unmarked.
            collect_file(&entry.path(), report, candidates)?;
            continue;
        };
        if digests.contains(&digest) {
            report.store_bytes += fs::metadata(entry.path())?.len();
            if namespace == "results" {
                report.marked_results += 1;
            } else {
                report.marked_objects += 1;
            }
            continue;
        }
        collect_file(&entry.path(), report, candidates)?;
    }
    Ok(())
}

/// Parses a sharded digest file name: the 2-hex shard directory plus the
/// 62-hex file name form the full 64-hex digest.
fn digest_from_name(shard: &str, name: &str) -> Option<Digest> {
    if name.len() != DIGEST_HEX_LEN - 2 || shard.len() != 2 {
        return None;
    }
    let mut hex = String::with_capacity(DIGEST_HEX_LEN);
    hex.push_str(shard);
    hex.push_str(name);
    Digest::from_hex(&hex).ok()
}

fn collect_file(path: &Path, report: &mut GcReport, candidates: &mut Vec<Candidate>) -> io::Result<()> {
    let metadata = fs::metadata(path)?;
    let mtime = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let size = metadata.len();
    report.store_bytes += size;
    candidates.push(Candidate {
        mtime_secs: mtime,
        size,
        path: path.to_path_buf(),
    });
    Ok(())
}

/// Whether an object's age meets the deletion floor. `None` or `ZERO`
/// deletes everything.
fn is_older_than(mtime_secs: u64, now: u64, older_than: Option<Duration>) -> bool {
    match older_than {
        None => true,
        Some(floor) => mtime_secs
            .checked_add(floor.as_secs())
            .is_none_or(|deadline| deadline <= now),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use tong_core::artifact::{BlobDigest, TreeDigest};
    use tong_core::canonical;
    use tong_core::digest::Hasher;
    use tong_core::platform::PlatformKey;
    use tong_core::tree::{Tree, TreeEntry};
    use crate::action_cache::ActionCache;
    use crate::state::BuildManifest;

    fn cas_with(action_digests: &[Digest]) -> (tempfile::TempDir, Cas, ActionCache) {
        let dir = tempfile::tempdir().unwrap();
        let cas = Cas::open(dir.path().join("store")).unwrap();
        let cache = ActionCache::open(&cas).unwrap();
        for digest in action_digests {
            let result = crate::action_cache::CachedResult {
                outputs: TreeDigest::new(Hasher::digest(b"outputs")),
                stdout: BlobDigest::new(Hasher::digest(b"stdout")),
                stderr: BlobDigest::new(Hasher::digest(b"stderr")),
                duration_millis: 1,
            };
            cache.put(*digest, &result).unwrap();
        }
        (dir, cas, cache)
    }

    fn blob_in(cas: &Cas, data: &[u8]) -> BlobDigest {
        cas.put_blob(data).unwrap()
    }

    fn tree_in(cas: &Cas, name: &str, blob: BlobDigest) -> TreeDigest {
        let tree = Tree::new(
            [(
                name.to_owned(),
                TreeEntry::File {
                    digest: blob,
                    executable: false,
                },
            )]
            .into_iter()
            .collect(),
        )
        .unwrap();
        cas.put_tree(&tree).unwrap()
    }

    fn bundle_in(cas: &Cas) -> Digest {
        let bundle = tong_core::bundle::EnvironmentBundle {
            name: "test".to_owned(),
            provider: "test".to_owned(),
            platform: PlatformKey::default(),
            variables: BTreeMap::new(),
            files: TreeDigest::new(Hasher::digest(b"files")),
            metadata: BTreeMap::new(),
        };
        cas.put_bundle(&bundle).unwrap()
    }

    fn manifest_marking(_cas: &Cas, action_digests: &[Digest]) -> BuildManifest {
        let actions = action_digests
            .iter()
            .map(|digest| crate::state::RecordedAction {
                action_digest: *digest,
                logical_id: "a".to_owned(),
                mnemonic: "m".to_owned(),
                input_root: TreeDigest::new(Hasher::digest(b"in")),
                executable: None,
                env_bundle: None,
                outputs: TreeDigest::new(Hasher::digest(b"out")),
                stdout: BlobDigest::new(Hasher::digest(b"so")),
                stderr: BlobDigest::new(Hasher::digest(b"se")),
                duration_millis: 0,
            })
            .collect();
        BuildManifest {
            schema_version: crate::state::BUILD_MANIFEST_SCHEMA_VERSION,
            project_hash: Hasher::digest(b"p"),
            created_at_unix_secs: 1,
            graph_digest: Hasher::digest(b"g"),
            profiles: vec!["dev".to_owned()],
            sources: vec![Hasher::digest(b"s")],
            toolchains: vec![Hasher::digest(b"t")],
            actions,
            artifacts: Vec::new(),
        }
    }

    fn age_file(path: &Path, age_secs: u64, now: u64) {
        use std::fs::FileTimes;
        let time = std::time::UNIX_EPOCH + Duration::from_secs(now - age_secs);
        let file = fs::File::open(path).unwrap();
        let _ = file.set_times(FileTimes::new().set_accessed(time).set_modified(time));
    }

    fn now_secs() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 1
    }

    #[test]
    fn sweep_deletes_only_unmarked_and_reports_counts() {
        let live = Hasher::digest(b"live");
        let dead = Hasher::digest(b"dead");
        let (_dir, cas, cache) = cas_with(&[live, dead]);
        let state = StateStore::open(cas.root()).unwrap();

        // A blob and tree only referenced by the dead action.
        let dead_blob = blob_in(&cas, b"dead-blob");
        let _dead_tree = tree_in(&cas, "f", dead_blob);
        let live_blob = blob_in(&cas, b"live-blob");
        let live_tree = tree_in(&cas, "f", live_blob);
        let _bundle = bundle_in(&cas);

        // The manifest's source tree keeps the live blob/tree alive.
        let mut manifest = manifest_marking(&cas, &[live]);
        manifest.sources = vec![live_tree.digest()];
        state.write(&manifest).unwrap();

        let now = now_secs();
        // Everything is old enough to be garbage.
        let opts = GcOptions {
            older_than: Some(Duration::ZERO),
            max_size: None,
            dry_run: false,
            now,
        };
        let report = sweep(&cas, &state, &opts).unwrap();
        assert_eq!(report.marked_results, 1);
        assert_eq!(report.deleted_results, 1);
        assert_eq!(report.deleted_objects, 3); // dead blob, dead tree, bundle
        assert!(report.freed_bytes > 0);
        assert!(cache.get(live).unwrap().is_some());
        assert!(cache.get(dead).unwrap().is_none());
        assert!(cas.blob_path(dead_blob).is_none());
        assert!(cas.blob_path(live_blob).is_some());
    }

    #[test]
    fn age_floor_keeps_recent_unmarked_objects() {
        let dead = Hasher::digest(b"dead2");
        let (_dir, cas, cache) = cas_with(&[dead]);
        let state = StateStore::open(cas.root()).unwrap();
        let now = now_secs();
        let opts = GcOptions {
            older_than: Some(Duration::from_secs(7 * 86_400)),
            max_size: None,
            dry_run: false,
            now,
        };
        let report = sweep(&cas, &state, &opts).unwrap();
        // The result file is fresh (mtime == now); retention keeps it.
        assert_eq!(report.deleted_results, 0);
        assert!(cache.get(dead).unwrap().is_some());
    }

    #[test]
    fn budget_sweep_respects_24h_floor_and_oldest_first() {
        let dead = Hasher::digest(b"dead3");
        let (_dir, cas, cache) = cas_with(&[dead]);
        let state = StateStore::open(cas.root()).unwrap();
        let now = now_secs();
        let result_path = cache
            .get(dead)
            .unwrap()
            .map(|_| {
                // Locate the result file path via the cache layout.
                let hex = dead.to_hex();
                cas.root().join("results").join(&hex[..2]).join(&hex[2..])
            })
            .unwrap();
        // One hour old: protected by the 24h floor.
        age_file(&result_path, 3600, now);
        let opts = GcOptions {
            // A long age floor isolates the budget sweep: nothing is
            // deleted by age, only by the budget.
            older_than: Some(Duration::from_secs(365 * 86_400)),
            max_size: Some(0),
            dry_run: false,
            now,
        };
        let report = sweep(&cas, &state, &opts).unwrap();
        assert_eq!(report.deleted_results, 0, "younger than 24h must survive");

        // Two days old: deleted by the budget sweep.
        age_file(&result_path, 2 * 86_400, now);
        let report = sweep(&cas, &state, &opts).unwrap();
        assert_eq!(report.deleted_results, 1);
        assert!(cache.get(dead).unwrap().is_none());
    }

    #[test]
    fn dry_run_reports_without_deleting() {
        let dead = Hasher::digest(b"dead4");
        let (_dir, cas, cache) = cas_with(&[dead]);
        let state = StateStore::open(cas.root()).unwrap();
        let opts = GcOptions {
            older_than: Some(Duration::ZERO),
            max_size: None,
            dry_run: true,
            now: now_secs(),
        };
        let report = sweep(&cas, &state, &opts).unwrap();
        assert_eq!(report.deleted_results, 1);
        assert!(cache.get(dead).unwrap().is_some());
    }

    #[test]
    fn marked_objects_survive() {
        // Regression: manifest-referenced blobs must survive even when
        // unmarked results are deleted.
        let live = Hasher::digest(b"live5");
        let (_dir, cas, cache) = cas_with(&[live]);
        let state = StateStore::open(cas.root()).unwrap();
        let blob = blob_in(&cas, b"kept");
        let tree = tree_in(&cas, "f", blob);
        // A manifest whose source tree is this tree.
        let mut manifest = manifest_marking(&cas, &[live]);
        manifest.sources = vec![tree.digest()];
        state.write(&manifest).unwrap();
        let opts = GcOptions {
            older_than: Some(Duration::ZERO),
            max_size: None,
            dry_run: false,
            now: 1_000_000,
        };
        let report = sweep(&cas, &state, &opts).unwrap();
        assert_eq!(report.deleted_results, 0);
        assert_eq!(report.deleted_objects, 0);
        assert!(cas.blob_path(blob).is_some());
        assert!(cache.get(live).unwrap().is_some());
    }

    #[test]
    fn tmp_leftovers_are_deleted() {
        let (_dir, cas, _) = cas_with(&[]);
        let state = StateStore::open(cas.root()).unwrap();
        let tmp = cas.root().join("tmp").join("stale");
        fs::write(&tmp, b"leftover").unwrap();
        age_file(&tmp, 2 * 86_400, 1_000_000);
        let opts = GcOptions {
            older_than: None,
            max_size: None,
            dry_run: false,
            now: 1_000_000,
        };
        let report = sweep(&cas, &state, &opts).unwrap();
        assert_eq!(report.deleted_objects, 1);
        assert!(!tmp.exists());
    }

    #[test]
    fn canonical_roundtrip_of_recorded_action() {
        let action = crate::state::RecordedAction {
            action_digest: Hasher::digest(b"a"),
            logical_id: "id".to_owned(),
            mnemonic: "m".to_owned(),
            input_root: TreeDigest::new(Hasher::digest(b"i")),
            executable: Some(BlobDigest::new(Hasher::digest(b"e"))),
            env_bundle: Some(Hasher::digest(b"b")),
            outputs: TreeDigest::new(Hasher::digest(b"o")),
            stdout: BlobDigest::new(Hasher::digest(b"so")),
            stderr: BlobDigest::new(Hasher::digest(b"se")),
            duration_millis: 9,
        };
        let bytes = canonical::encode_vec(&action);
        let decoded = canonical::decode_all::<crate::state::RecordedAction>(&bytes).unwrap();
        assert_eq!(decoded, action);
    }
}
