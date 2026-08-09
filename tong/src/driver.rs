//! The build driver: manifests → toolchain → plan → schedule → execute →
//! assemble (PLAN.md section 15, Phase 1 pipeline).

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use tong_core::action::ActionId;
use tong_core::artifact::TreeDigest;
use tong_core::units::{parse_duration, parse_size};
use tong_exec::{ExecError, LocalExecutor};
use tong_graph::manifest::Manifest;
use tong_graph::{Completed, PlanError, topological_order};
use tong_rust::{RustBackend, capture_system_rust, import_cargo_workspace};
use tong_store::{
    ActionCache, CachedResult, Cas, GcOptions, GcReport, StateStore, graph_digest, project_hash,
    sweep,
};

use crate::manifest_mode::manifest_to_model;

/// Default retention for unmarked cache objects (auto-GC after builds).
pub const DEFAULT_RETENTION: &str = "7d";
/// Default store size budget (auto-GC after builds).
pub const DEFAULT_MAX_SIZE: &str = "10G";

/// Resolves the store directory for a workspace.
///
/// Resolution order: env `TONG_STORE_DIR` → `[store] dir` in `Tong.toml`
/// (relative to the workspace root; native mode only) → `<root>/.tong/store`
/// (the project-local default). Everything else (exec roots, `out/`) stays
/// under `<root>/.tong/` in both modes.
pub fn store_dir(root: &Path, manifest: Option<&Manifest>) -> Result<PathBuf, BuildError> {
    if let Some(dir) = std::env::var_os("TONG_STORE_DIR") {
        return Ok(PathBuf::from(dir));
    }
    if let Some(store) = manifest.and_then(|manifest| manifest.store.as_ref())
        && let Some(dir) = &store.dir
    {
        let path = Path::new(dir);
        if path.is_absolute()
            || path
                .components()
                .any(|c| c == std::path::Component::ParentDir)
        {
            return Err(BuildError::Store(format!(
                "[store] dir {dir:?} must be a relative path without `..` components"
            )));
        }
        return Ok(root.join(path));
    }
    Ok(root.join(".tong").join("store"))
}

/// Resolves the GC retention policy: `TONG_STORE_RETENTION` →
/// `[store] retention` → default [`DEFAULT_RETENTION`].
pub fn retention_policy(manifest: Option<&Manifest>) -> Result<std::time::Duration, BuildError> {
    let text = std::env::var("TONG_STORE_RETENTION")
        .ok()
        .or_else(|| {
            manifest
                .and_then(|manifest| manifest.store.as_ref())
                .and_then(|store| store.retention.clone())
        })
        .unwrap_or_else(|| DEFAULT_RETENTION.to_owned());
    parse_duration(&text).map_err(|err| BuildError::Store(err.to_string()))
}

/// Resolves the store size budget: `TONG_STORE_MAX_SIZE` → `[store]
/// max_size` → default [`DEFAULT_MAX_SIZE`].
pub fn max_size_policy(manifest: Option<&Manifest>) -> Result<u64, BuildError> {
    let text = std::env::var("TONG_STORE_MAX_SIZE")
        .ok()
        .or_else(|| {
            manifest
                .and_then(|manifest| manifest.store.as_ref())
                .and_then(|store| store.max_size.clone())
        })
        .unwrap_or_else(|| DEFAULT_MAX_SIZE.to_owned());
    parse_size(&text).map_err(|err| BuildError::Store(err.to_string()))
}

/// Build driver options.
#[derive(Clone, Debug)]
pub struct BuildOptions {
    /// Profile name.
    pub profile: String,
    /// Restrict materialized artifacts to these target names.
    pub targets: Vec<String>,
}

/// Build outcome.
#[derive(Debug, Default)]
pub struct BuildOutcome {
    /// Actions in the graph.
    pub actions_total: usize,
    /// Actions satisfied from the cache.
    pub actions_cached: usize,
    /// Actions executed.
    pub actions_executed: usize,
    /// Materialized artifacts.
    pub artifacts: Vec<PathBuf>,
}

/// Driver failure.
#[derive(Debug)]
pub enum BuildError {
    /// The workspace has neither `Tong.toml` nor a usable `Cargo.toml`.
    NoManifest,
    /// Manifest loading or import failed.
    Manifest(String),
    /// Toolchain capture failed.
    Toolchain(tong_rust::ToolchainError),
    /// Planning or concretization failed.
    Plan(PlanError),
    /// A cycle was detected.
    Cycle(String),
    /// Execution failed.
    Exec(ExecError),
    /// Store configuration or GC failure.
    Store(String),
    /// I/O failure.
    Io(io::Error),
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoManifest => {
                write!(f, "no Tong.toml or Cargo.toml found in the workspace")
            }
            Self::Manifest(msg) => write!(f, "{msg}"),
            Self::Toolchain(err) => write!(f, "{err}"),
            Self::Plan(err) => write!(f, "{err}"),
            Self::Cycle(msg) => write!(f, "{msg}"),
            Self::Exec(err) => write!(f, "{err}"),
            Self::Store(msg) => write!(f, "{msg}"),
            Self::Io(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for BuildError {}

impl From<io::Error> for BuildError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<PlanError> for BuildError {
    fn from(err: PlanError) -> Self {
        Self::Plan(err)
    }
}

impl From<tong_rust::ToolchainError> for BuildError {
    fn from(err: tong_rust::ToolchainError) -> Self {
        Self::Toolchain(err)
    }
}

/// Builds the workspace at `root` and materializes artifacts under
/// `.tong/out/<profile>/`.
pub fn build(root: &Path, options: &BuildOptions) -> Result<BuildOutcome, BuildError> {
    let tong_dir = root.join(".tong");
    let manifest = load_manifest(root)?;
    let store = store_dir(root, manifest.as_ref())?;
    let exec = tong_dir.join("exec");
    let cas = Cas::open(&store)?;
    let cache = ActionCache::open(&cas)?;

    // Model first: manifest errors fail fast, before the expensive system
    // toolchain capture (rustc import + sysroot fingerprinting).
    let model = load_model(root, manifest.as_ref())?;

    // Build-start hygiene: prune stale exec roots. Exec content is fully
    // reproducible (everything is in the CAS); failed builds keep their
    // roots until the next build, which is the diagnosis window.
    prune_exec_dir(&exec)?;

    // Toolchain: needed by the backend for action identity.
    let toolchain = capture_system_rust(&cas)?;

    let mut executor = LocalExecutor::new(cas.clone(), &exec)?;
    executor.register_system_tool(toolchain.rustc_blob, toolchain.rustc.clone());
    executor.register_bundle_root(toolchain.bundle.digest(), toolchain.root.clone());

    // Plan.
    let mut backend = RustBackend::new(cas.clone(), &model, toolchain, &options.profile)?;
    let planned = backend.plan()?;
    let artifacts = backend.final_artifacts();

    let order = match topological_order(&planned) {
        Ok(order) => order,
        Err(cycle) => {
            return Err(BuildError::Cycle(format!(
                "action cycle: {:?}",
                cycle.remaining
            )));
        }
    };

    // Schedule: concretize, check cache, execute.
    let mut completed = CompletedMap(BTreeMap::new());
    let mut recorded: Vec<tong_store::RecordedAction> = Vec::new();
    let mut graph_pairs: BTreeMap<String, tong_core::digest::Digest> = BTreeMap::new();
    let mut sources: Vec<tong_core::digest::Digest> = Vec::new();
    let mut toolchains: Vec<tong_core::digest::Digest> = Vec::new();
    let mut outcome = BuildOutcome {
        actions_total: order.len(),
        ..Default::default()
    };

    for (index, action) in order.iter().enumerate() {
        let spec = (action.make)(&completed, &cas)?;
        let digest = spec.digest();
        let cached = if let Some(result) = cache.get(digest)? {
            outcome.actions_cached += 1;
            println!(
                "  [{}/{}] {} ({}) [cached]",
                index + 1,
                order.len(),
                spec.logical_id.0,
                spec.mnemonic
            );
            result
        } else {
            println!(
                "  [{}/{}] {} ({})",
                index + 1,
                order.len(),
                spec.logical_id.0,
                spec.mnemonic
            );
            let result = match executor.execute(&spec) {
                Ok(outcome) => outcome,
                Err(ExecError::Exit { code, stderr, .. }) => {
                    let stderr_text = cas
                        .read_blob(stderr)
                        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                        .unwrap_or_default();
                    eprintln!("action {} failed with exit code {code}", spec.logical_id.0);
                    eprintln!("{stderr_text}");
                    return Err(BuildError::Exec(ExecError::Exit {
                        code,
                        stderr,
                        exec_root: PathBuf::new(),
                    }));
                }
                Err(err) => return Err(BuildError::Exec(err)),
            };
            let cached = CachedResult {
                outputs: result.outputs,
                stdout: result.stdout,
                stderr: result.stderr,
                duration_millis: result.duration.as_millis() as u64,
            };
            cache.put(digest, &cached)?;
            outcome.actions_executed += 1;
            cached
        };
        // Record for the build-state manifest (GC root set).
        graph_pairs.insert(spec.logical_id.0.clone(), digest);
        sources.push(spec.input_root.digest());
        if let Some(reference) = &spec.environment_bundle {
            toolchains.push(reference.digest());
        }
        recorded.push(tong_store::RecordedAction {
            action_digest: digest,
            logical_id: spec.logical_id.0.clone(),
            mnemonic: spec.mnemonic.clone(),
            input_root: spec.input_root,
            executable: match &spec.executable {
                tong_core::artifact::ArtifactRef::Blob(blob) => Some(*blob),
                _ => None,
            },
            env_bundle: spec.environment_bundle.as_ref().map(|r| r.digest()),
            outputs: cached.outputs,
            stdout: cached.stdout,
            stderr: cached.stderr,
            duration_millis: cached.duration_millis,
        });
        completed.0.insert(spec.logical_id.clone(), cached);
    }

    // Assemble requested final artifacts.
    let out_dir = tong_dir.join("out").join(&options.profile);
    let requested: Vec<&tong_rust::FinalArtifact> = artifacts
        .iter()
        .filter(|artifact| {
            options.targets.is_empty()
                || options
                    .targets
                    .iter()
                    .any(|t| artifact_name_matches(t, &artifact.name))
        })
        .collect();
    let mut artifact_pairs: Vec<(String, TreeDigest)> = Vec::new();
    for artifact in requested {
        let Some(result) = completed.0.get(&artifact.action) else {
            continue;
        };
        let dest = out_dir.join(&artifact.name);
        fs::create_dir_all(&dest)?;
        cas.materialize(result.outputs, &dest)?;
        for (blob, name) in &artifact.runtime {
            let blob_path = cas.blob_path(*blob).ok_or_else(|| {
                BuildError::Io(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("runtime blob {} missing", blob.digest()),
                ))
            })?;
            let target = dest.join(name);
            // Copy, never hard-link: artifacts are independent files; a
            // rewrite of the artifact must never be able to corrupt the
            // immutable store blob (a same-inode copy truncates it).
            if target.exists() {
                fs::remove_file(&target)?;
            }
            fs::copy(&blob_path, &target)?;
        }
        artifact_pairs.push((artifact.name.clone(), result.outputs));
        outcome.artifacts.push(dest.join(&artifact.name));
    }

    // Record the build-state manifest (the GC root set) and run the
    // automatic GC. Both are best-effort: cache correctness is unaffected,
    // and a failed write leaves the previous manifest in place.
    if let Ok(project_hash) = project_hash(root) {
        let state = StateStore::open(&store)?;
        let build_manifest = tong_store::BuildManifest {
            schema_version: tong_store::BUILD_MANIFEST_SCHEMA_VERSION,
            project_hash,
            created_at_unix_secs: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            graph_digest: graph_digest(&graph_pairs),
            profiles: vec![options.profile.clone()],
            sources,
            toolchains,
            actions: recorded,
            artifacts: artifact_pairs,
        };
        match state.write(&build_manifest) {
            Ok(()) => {
                let retention = retention_policy(manifest.as_ref())?;
                let max_size = max_size_policy(manifest.as_ref())?;
                let report = sweep(
                    &cas,
                    &state,
                    &GcOptions {
                        older_than: Some(retention),
                        max_size: Some(max_size),
                        dry_run: false,
                        now: std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs())
                            .unwrap_or(0),
                    },
                );
                match report {
                    Ok(report) => println!("{report}"),
                    Err(err) => eprintln!("tong: warning: automatic GC failed: {err}"),
                }
            }
            Err(err) => eprintln!(
                "tong: warning: could not record build state ({}); GC will keep the previous manifest",
                err
            ),
        }
    }

    Ok(outcome)
}

/// Removes every entry of the exec directory (stale exec roots from failed
/// or interrupted builds; content is reproducible from the CAS).
fn prune_exec_dir(exec: &Path) -> io::Result<()> {
    let entries = match fs::read_dir(exec) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            fs::remove_dir_all(&path)?;
        } else {
            fs::remove_file(&path)?;
        }
    }
    Ok(())
}

/// Loads `Tong.toml` when present (`None` in Cargo-import mode).
fn load_manifest(root: &Path) -> Result<Option<Manifest>, BuildError> {
    if root.join("Tong.toml").exists() {
        let manifest = Manifest::load(root).map_err(|err| BuildError::Manifest(err.to_string()))?;
        Ok(Some(manifest))
    } else {
        Ok(None)
    }
}

/// Loads the Rust model: `Tong.toml` if present, else Cargo import.
pub fn load_model(
    root: &Path,
    manifest: Option<&Manifest>,
) -> Result<tong_rust::RustModel, BuildError> {
    if let Some(manifest) = manifest {
        Ok(manifest_to_model(manifest, root))
    } else if root.join("Cargo.toml").exists() {
        import_cargo_workspace(root).map_err(|err| BuildError::Manifest(err.to_string()))
    } else {
        Err(BuildError::NoManifest)
    }
}

/// Runs a built binary target with the given arguments.
pub fn run(root: &Path, target: &str, args: &[String], profile: &str) -> Result<i32, BuildError> {
    let options = BuildOptions {
        profile: profile.to_owned(),
        targets: vec![target.to_owned()],
    };
    let outcome = build(root, &options)?;
    let binary = outcome
        .artifacts
        .first()
        .ok_or_else(|| BuildError::Manifest(format!("target {target:?} produced no artifact")))?;
    let status = std::process::Command::new(binary)
        .args(args)
        .current_dir(root)
        .status()
        .map_err(BuildError::Io)?;
    Ok(status.code().unwrap_or(1))
}

/// Matches a target label (`:name`, `//path:name`, or `name`) against an
/// artifact name. An exact match wins; otherwise underscores and dashes
/// are treated as equal, so `:voxel_city` finds a Cargo package named
/// `voxel-city` (Cargo sanitizes crate names; Tong.toml names don't).
fn artifact_name_matches(label: &str, name: &str) -> bool {
    let label = label
        .strip_prefix(':') //
        .or_else(|| {
            label
                .strip_prefix("//")
                .and_then(|rest| rest.rsplit_once(':').map(|(_, name)| name))
        })
        .unwrap_or(label);
    if label == name {
        return true;
    }
    label.replace('_', "-") == name.replace('_', "-")
}

/// Removes the project-local `.tong` directory, or — in shared-store mode —
/// the project's exec roots, outputs, state manifests, and unreferenced
/// store objects.
pub fn clean(root: &Path) -> Result<(), BuildError> {
    let tong_dir = root.join(".tong");
    let manifest = load_manifest(root)?;
    let store = store_dir(root, manifest.as_ref())?;
    let project_local = store == root.join(".tong").join("store");

    if project_local {
        // The store *is* the project's cache: `tong clean` deletes it all.
        if tong_dir.exists() {
            fs::remove_dir_all(&tong_dir)?;
        }
        return Ok(());
    }

    // Shared mode: remove the project-local state, then drop this
    // project's objects that no other project's manifest marks.
    for sub in ["exec", "out"] {
        let dir = tong_dir.join(sub);
        if dir.exists() {
            fs::remove_dir_all(&dir)?;
        }
    }
    let cas = Cas::open(&store)?;
    let state = StateStore::open(&store)?;
    if let Ok(project_hash) = project_hash(root) {
        state.remove_project(&project_hash)?;
        let report = sweep(
            &cas,
            &state,
            &GcOptions {
                older_than: Some(std::time::Duration::ZERO),
                max_size: None,
                dry_run: false,
                now: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
            },
        )?;
        println!("{report}");
    }
    Ok(())
}

/// Garbage-collects the store: deletes unmarked objects older than the
/// retention floor (or the `--older-than` override) and, when configured,
/// sweeps the store under the size budget.
pub fn gc(root: &Path, opts: &GcCli) -> Result<GcReport, BuildError> {
    let manifest = load_manifest(root)?;
    let store = store_dir(root, manifest.as_ref())?;
    let cas = Cas::open(&store)?;
    let state = StateStore::open(&store)?;
    let older_than = match &opts.older_than {
        Some(text) => Some(parse_duration(text).map_err(|err| BuildError::Store(err.to_string()))?),
        None => Some(retention_policy(manifest.as_ref())?),
    };
    let max_size = match &opts.max_size {
        Some(text) => Some(parse_size(text).map_err(|err| BuildError::Store(err.to_string()))?),
        None => Some(max_size_policy(manifest.as_ref())?),
    };
    let report = sweep(
        &cas,
        &state,
        &GcOptions {
            older_than,
            max_size,
            dry_run: opts.dry_run,
            now: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        },
    )?;
    println!("{report}");
    Ok(report)
}

/// `tong gc` command-line options.
#[derive(Clone, Debug, Default)]
pub struct GcCli {
    /// Delete unmarked objects older than this duration (`0` = all).
    pub older_than: Option<String>,
    /// Store size budget (`10G`, `500M`).
    pub max_size: Option<String>,
    /// Report without deleting.
    pub dry_run: bool,
}

impl Completed for CompletedMap {
    fn output_tree(&self, action: &ActionId) -> Option<TreeDigest> {
        self.0.get(action).map(|result| result.outputs)
    }

    fn stdout(&self, action: &ActionId) -> Option<tong_core::artifact::BlobDigest> {
        self.0.get(action).map(|result| result.stdout)
    }

    fn stderr(&self, action: &ActionId) -> Option<tong_core::artifact::BlobDigest> {
        self.0.get(action).map(|result| result.stderr)
    }
}

/// Completed actions keyed by logical id (newtype to satisfy the orphan
/// rule for the [`Completed`] trait).
struct CompletedMap(BTreeMap<ActionId, CachedResult>);

#[cfg(test)]
mod tests {
    use super::artifact_name_matches;

    #[test]
    fn artifact_labels_match_exactly_or_with_normalized_separators() {
        assert!(artifact_name_matches(":voxel_city", "voxel_city"));
        assert!(artifact_name_matches(":voxel_city", "voxel-city"));
        assert!(artifact_name_matches(":voxel-city", "voxel_city"));
        assert!(artifact_name_matches("//crates/app:calc-cli", "calc-cli"));
        assert!(artifact_name_matches("calc-cli", "calc-cli"));
        assert!(!artifact_name_matches(":other", "voxel-city"));
    }
}
