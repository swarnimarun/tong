//! The build driver: manifests → toolchain → plan → schedule → execute →
//! assemble (PLAN.md section 15, Phase 1 pipeline).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use tong_core::action::{ActionId, CachePolicy};
use tong_core::artifact::TreeDigest;
use tong_core::units::{parse_duration, parse_size};
use tong_exec::{ExecError, LocalExecutor};
use tong_graph::manifest::Manifest;
use tong_graph::{Completed, PlanError, topological_order};
use tong_rust::{
    RustBackend, SystemRust, ToolchainError, capture_system_rust, import_cargo_workspace,
};
use tong_store::{
    ActionCache, BuildManifest, CachedResult, Cas, GcOptions, GcReport, StateStore, graph_digest,
    project_hash, sweep,
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
#[derive(Clone, Debug, Default)]
pub struct BuildOptions {
    /// Profile name.
    pub profile: String,
    /// Restrict materialized artifacts to these target names.
    pub targets: Vec<String>,
    /// Feature selection (Cargo-style flags).
    pub features: FeatureOptions,
    /// Sandbox enforcement level (`[policy] sandbox`, default `l1`).
    pub sandbox: Option<tong_exec::SandboxLevel>,
    /// Execute only actions owned by non-workspace packages (registry,
    /// git, and path dependencies); workspace actions are skipped entirely
    /// and nothing is assembled. Docker dep layers: busts only when the
    /// lockfile or toolchain changes (docs/docker-caching.md).
    pub deps_only: bool,
    /// Never touch the network: a missing or outdated `Tong.lock`, absent
    /// sources, or an absent pinned toolchain fail with a targeted
    /// diagnostic instead of being fetched. Auto-lock/auto-fetch are
    /// disabled; `tong lock`/`tong fetch` run separately.
    pub offline: bool,
    /// Forbid rewriting `Tong.lock`: a missing lock (auto-lock disabled)
    /// or an outdated one fails instead of being regenerated. `--frozen`
    /// is exactly `--locked --offline`.
    pub locked: bool,
}

/// Feature selection for a build (`--features`, `--no-default-features`,
/// `--all-features`).
#[derive(Clone, Debug, Default)]
pub struct FeatureOptions {
    /// Features to activate on the selected packages.
    pub features: Vec<String>,
    /// Disable the default feature of the selected packages.
    pub no_default_features: bool,
    /// Activate every declared feature of the selected packages.
    pub all_features: bool,
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
    /// Actions skipped (workspace actions under `--deps-only`).
    pub actions_skipped: usize,
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
    /// The build needs a lockfile or fetched sources it does not have.
    Offline(String),
    /// `tong dockerfile` generation failed.
    Dockerfile(String),
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
            Self::Offline(msg) => write!(f, "{msg}"),
            Self::Dockerfile(msg) => write!(f, "{msg}"),
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
    let t_build = std::time::Instant::now();
    let prepared = prepare(root, options, false, false, &[])?;
    let tong_dir = &prepared.tong_dir;
    let cas = &prepared.cas;
    let cache = &prepared.cache;
    let executor = &prepared.executor;
    let order = &prepared.order;

    // Schedule: concretize, check cache, execute.
    let mut completed = CompletedMap(BTreeMap::new());
    let mut recorded: Vec<tong_store::RecordedAction> = Vec::new();
    let mut graph_pairs: BTreeMap<String, tong_core::digest::Digest> = BTreeMap::new();
    let mut sources: Vec<tong_core::digest::Digest> = Vec::new();
    if options.deps_only {
        // The deps-only manifest records every captured package tree: the
        // deps stage captured the local packages' manifest-only trees, and
        // the app stage re-captures identical digests — GC must keep them
        // (docs/docker-caching.md Feature 1).
        sources.extend(prepared.source_trees.iter().copied());
    }
    let mut toolchains: Vec<tong_core::digest::Digest> = Vec::new();
    let mut outcome = BuildOutcome {
        actions_total: order.len(),
        ..Default::default()
    };

    for index in 0..order.len() {
        let action = &prepared.planned[order[index]];
        // `--deps-only`: skip workspace-owned actions entirely — no cache
        // lookup, no execution, no recording. External actions never
        // depend on workspace actions, so the topological order stays
        // valid.
        if options.deps_only && !action.external {
            outcome.actions_skipped += 1;
            continue;
        }
        let spec = (action.make)(&completed, cas)?;
        let digest = spec.digest();
        // `CachePolicy::NoCache` actions (test runs, network-allowed
        // actions) must bypass the action cache entirely: no lookup, no
        // insertion. Their digest still covers the policy, so a NoCache
        // action can never alias a cacheable one.
        let cacheable = spec.cache_policy == CachePolicy::Enabled;
        let cached = if cacheable && let Some(result) = cache.get(digest)? {
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
            tracing::debug!(
                target: "tong::perf",
                phase = "action.execute",
                action = %spec.logical_id.0,
                duration_ms = result.duration.as_millis() as u64,
            );
            let cached = CachedResult {
                outputs: result.outputs,
                stdout: result.stdout,
                stderr: result.stderr,
                duration_millis: result.duration.as_millis() as u64,
            };
            if cacheable {
                cache.put(digest, &cached)?;
            }
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

    // Assemble requested final artifacts. `--deps-only` assembles nothing:
    // workspace artifacts are not built, and dep artifacts are consumed by
    // the app stage's cache hits.
    let out_dir = tong_dir.join("out").join(&options.profile);
    tracing::debug!(
        target: "tong::perf",
        phase = "schedule",
        actions = order.len(),
        cached = outcome.actions_cached,
        executed = outcome.actions_executed,
        skipped = outcome.actions_skipped,
        duration_ms = t_build.elapsed().as_millis() as u64,
    );
    let t_assemble = std::time::Instant::now();
    let mut artifact_pairs: Vec<(String, TreeDigest)> = Vec::new();
    if !options.deps_only {
        let requested: Vec<&tong_rust::FinalArtifact> = prepared
            .artifacts
            .iter()
            .filter(|artifact| {
                options.targets.is_empty()
                    || options
                        .targets
                        .iter()
                        .any(|t| artifact_name_matches(t, &artifact.name))
            })
            .collect();
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
    }
    tracing::debug!(
        target: "tong::perf",
        phase = "assemble",
        duration_ms = t_assemble.elapsed().as_millis() as u64,
    );
    let t_record = std::time::Instant::now();

    record_state(
        root,
        &prepared,
        &recorded,
        &graph_pairs,
        &sources,
        &toolchains,
        &artifact_pairs,
        &options.profile,
        options.deps_only,
    )?;
    tracing::debug!(
        target: "tong::perf",
        phase = "record_state",
        duration_ms = t_record.elapsed().as_millis() as u64,
    );
    tracing::debug!(
        target: "tong::perf",
        phase = "build.total",
        duration_ms = t_build.elapsed().as_millis() as u64,
    );

    Ok(outcome)
}

/// Runs the workspace's test targets (`tong test`). Returns the process
/// exit code: 0 when every executed test suite passed, 1 on the first
/// failing suite.
pub fn test(
    root: &Path,
    label: Option<&str>,
    libtest_args: &[String],
    options: &BuildOptions,
) -> Result<i32, BuildError> {
    let prepared = prepare(root, options, true, true, libtest_args)?;
    let cas = &prepared.cas;
    let cache = &prepared.cache;
    let executor = &prepared.executor;
    let order = &prepared.order;

    let mut completed = CompletedMap(BTreeMap::new());
    let mut recorded: Vec<tong_store::RecordedAction> = Vec::new();
    let mut graph_pairs: BTreeMap<String, tong_core::digest::Digest> = BTreeMap::new();
    let mut sources: Vec<tong_core::digest::Digest> = Vec::new();
    let mut toolchains: Vec<tong_core::digest::Digest> = Vec::new();
    // Executed test runs in (label, stdout blob) order.
    let mut test_runs: Vec<(String, tong_core::artifact::BlobDigest)> = Vec::new();
    let mut failed = false;

    for index in 0..order.len() {
        let action = &prepared.planned[order[index]];
        let spec = (action.make)(&completed, cas)?;
        let digest = spec.digest();

        // Test runs: skipped when they do not match the label filter.
        let is_test_run = spec.logical_id.0.starts_with("rust:test-run:");
        if is_test_run
            && let Some(label) = label
            && !test_run_matches(&spec.logical_id.0, label)
        {
            continue;
        }

        // Test runs are `CachePolicy::NoCache`: never look up or insert —
        // every run re-executes (deterministic native tests opt into
        // caching via `cache_test_result = true`).
        let cacheable = spec.cache_policy == CachePolicy::Enabled;
        let cached = if cacheable && let Some(result) = cache.get(digest)? {
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
                    eprintln!("test {} failed with exit code {code}", spec.logical_id.0);
                    eprintln!("{stderr_text}");
                    if is_test_run {
                        failed = true;
                        break;
                    }
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
            if cacheable {
                cache.put(digest, &cached)?;
            }
            cached
        };
        if is_test_run {
            test_runs.push((spec.logical_id.0.clone(), cached.stdout));
        }
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

    // Summary: parse the libtest result lines from each executed suite.
    let mut passed = 0usize;
    let mut failed_tests = 0usize;
    for (id, stdout) in &test_runs {
        let text = cas
            .read_blob(*stdout)
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default();
        let result_line = text
            .lines()
            .find(|line| line.trim_start().starts_with("test result:"))
            .unwrap_or("test result: (no summary)");
        println!("{result_line} ({id})");
        let tokens: Vec<&str> = result_line.split_whitespace().collect();
        for (index, token) in tokens.iter().enumerate() {
            let number = tokens
                .get(index.wrapping_sub(1))
                .and_then(|n| n.parse::<usize>().ok());
            if token.starts_with("passed") {
                passed += number.unwrap_or(0);
            } else if token.starts_with("failed") {
                failed_tests += number.unwrap_or(0);
            }
        }
    }
    println!();
    println!("tests: {passed} passed, {failed_tests} failed");

    record_state(
        root,
        &prepared,
        &recorded,
        &graph_pairs,
        &sources,
        &toolchains,
        &[],
        &options.profile,
        false,
    )?;

    Ok(if failed || failed_tests > 0 { 1 } else { 0 })
}

/// Whether a test-run logical id (`rust:test-run:<pkg>:<name>`) matches a
/// label: the test name, the package name (all its tests), or `pkg:name`.
fn test_run_matches(logical_id: &str, label: &str) -> bool {
    let rest = logical_id
        .strip_prefix("rust:test-run:")
        .unwrap_or(logical_id);
    let (pkg, name) = rest.split_once(':').unwrap_or((rest, ""));
    let label = label
        .strip_prefix(':')
        .or_else(|| {
            label
                .strip_prefix("//")
                .and_then(|rest| rest.rsplit_once(':').map(|(_, name)| name))
        })
        .unwrap_or(label);
    label == name
        || label == pkg
        || label == format!("{pkg}:{name}")
        || artifact_name_matches(label, name)
}

/// Everything a build or test run needs after planning: the open store,
/// resolved model, executor, and the planned action graph.
struct Prepared {
    tong_dir: PathBuf,
    store: PathBuf,
    manifest: Option<Manifest>,
    cas: Cas,
    cache: ActionCache,
    executor: LocalExecutor,
    planned: Vec<tong_graph::PlannedAction>,
    artifacts: Vec<tong_rust::FinalArtifact>,
    /// Captured source-tree digests of every package (used by the
    /// deps-only manifest so GC keeps the local packages' trees).
    source_trees: Vec<tong_core::digest::Digest>,
    /// Topological order as indices into `planned`.
    order: Vec<usize>,
}

/// Shared build/test setup: store → model → features → toolchain → plan.
fn prepare(
    root: &Path,
    options: &BuildOptions,
    include_dev_deps: bool,
    tests_enabled: bool,
    test_args: &[String],
) -> Result<Prepared, BuildError> {
    let t_prep = std::time::Instant::now();
    let tong_dir = root.join(".tong");
    let manifest = load_manifest(root)?;
    // Auto-lock (cargo generates Cargo.lock on build; tong does the same
    // for Tong.lock): a missing lock is not an error — resolve it first
    // and say so. `--offline`/`--locked` forbid creating the lock (that
    // is a network request); fail with a targeted diagnostic instead.
    if manifest.is_none() && root.join("Cargo.toml").is_file() && !root.join("Tong.lock").is_file()
    {
        if options.offline || options.locked {
            return Err(BuildError::Offline(
                "no Tong.lock; run `tong lock` first \
                 (--offline/--locked forbid auto-locking)"
                    .to_owned(),
            ));
        }
        println!("tong: no Tong.lock — running `tong lock` first");
        lock(root, false)?;
    }
    let store = store_dir(root, manifest.as_ref())?;
    let exec = tong_dir.join("exec");
    let cas = Cas::open(&store)?;
    // Auto-fetch (cargo downloads sources as needed; tong does the same):
    // when a locked registry archive or git source tree is missing from
    // the store, fetch first and say so. `--offline` forbids the download
    // — fail naming every missing source instead.
    let missing_sources: Vec<String> = tong_fetch::TongLock::load(root)
        .ok()
        .into_iter()
        .flat_map(|lock| lock.packages)
        .filter(|pkg| {
            if pkg.source.starts_with("registry+") {
                pkg.checksum.as_deref().is_some_and(|checksum| {
                    !store
                        .join("sources")
                        .join(format!("{checksum}.crate"))
                        .is_file()
                })
            } else if pkg.source.starts_with("git+") {
                pkg.tree_digest.as_deref().is_some_and(|tree| {
                    tong_core::digest::Digest::from_hex(tree)
                        .ok()
                        .and_then(|digest| cas.get_tree(TreeDigest::new(digest)).ok().flatten())
                        .is_none()
                })
            } else {
                false
            }
        })
        .map(|pkg| format!("{} {}", pkg.name, pkg.version))
        .collect();
    if !missing_sources.is_empty() {
        if options.offline {
            return Err(BuildError::Offline(format!(
                "sources not fetched: missing {}; \
                 run `tong fetch` (--offline forbids downloading)",
                missing_sources.join(", ")
            )));
        }
        println!("tong: sources not fetched — running `tong fetch` first");
        fetch(root, false)?;
    }
    let cache = ActionCache::open(&cas)?;
    tracing::debug!(
        target: "tong::perf",
        phase = "prepare.open",
        duration_ms = t_prep.elapsed().as_millis() as u64,
    );

    // Model first: manifest errors fail fast, before the expensive system
    // toolchain capture (rustc query + sysroot fingerprinting). Registry
    // deps resolve against Tong.lock + the source store. The system
    // capture runs concurrently with model loading: both are read-only,
    // and CAS writes are atomic. A pinned dist version skips the capture
    // and is loaded after (store-only, fast).
    let dist_version = manifest
        .as_ref()
        .and_then(|manifest| manifest.toolchain.rust.version.as_deref());
    let (mut model, feature_map, toolchain, sources) = std::thread::scope(
        |scope| -> Result<
            (
                tong_rust::RustModel,
                tong_rust::FeatureMap,
                SystemRust,
                LockfileSource,
            ),
            BuildError,
        > {
            let capture = dist_version.is_none().then(|| {
                let cas_clone = cas.clone();
                scope.spawn(move || capture_system_rust(&cas_clone))
            });

            let sources = LockfileSource::new(root, &store, cas.clone());
            let model = load_model(root, manifest.as_ref(), &sources)?;
            tracing::debug!(
                target: "tong::perf",
                phase = "prepare.model",
                duration_ms = t_prep.elapsed().as_millis() as u64,
            );

            // Resolve features (Cargo resolver-v2 semantics) before
            // planning so `--cfg feature=...` flags and optional-dep edges
            // are baked into the action graph. Test builds additionally
            // activate dev-dep edges.
            let requests = feature_requests(&model, options, manifest.as_ref())?;
            let feature_map = tong_rust::resolve_features(&model, &requests, include_dev_deps)
                .map_err(|err| BuildError::Manifest(err.to_string()))?;

            // Build-start hygiene: prune stale exec roots. Exec content is
            // fully reproducible (everything is in the CAS); failed builds
            // keep their roots until the next build, which is the diagnosis
            // window.
            prune_exec_dir(&exec)?;

            let toolchain = match capture {
                Some(handle) => handle
                    .join()
                    .map_err(|_| {
                        BuildError::Toolchain(ToolchainError::Missing(
                            "system toolchain capture thread panicked".to_owned(),
                        ))
                    })?
                    .map_err(BuildError::Toolchain),
                None => {
                    let host_triple = tong_rust::host_triple()?;
                    tong_rust::load_dist_rust(&cas, &store, dist_version.unwrap(), &host_triple)
                        .map_err(BuildError::Toolchain)
                }
            }?;
            Ok((model, feature_map, toolchain, sources))
        },
    )?;
    model.feature_map = feature_map;
    tracing::debug!(
        target: "tong::perf",
        phase = "prepare.toolchain",
        duration_ms = t_prep.elapsed().as_millis() as u64,
    );

    // Sandbox level: `BuildOptions.sandbox` wins, then `[policy] sandbox`
    // (default l1 — opt-in).
    let sandbox_level = options
        .sandbox
        .or_else(|| {
            manifest
                .as_ref()
                .and_then(|manifest| manifest.policy.as_ref())
                .and_then(|policy| policy.sandbox.as_deref())
                .and_then(tong_exec::SandboxLevel::parse)
        })
        .unwrap_or(tong_exec::SandboxLevel::L1);

    let mut executor = LocalExecutor::with_sandbox(cas.clone(), &exec, sandbox_level)?;
    executor.register_system_tool(toolchain.rustc_blob, toolchain.rustc.clone());
    executor.register_bundle_root(toolchain.bundle.digest(), toolchain.root.clone());

    // `[policy] network = "allow"`: run actions may reach the network and
    // are uncacheable.
    let network_allow = manifest
        .as_ref()
        .and_then(|manifest| manifest.policy.as_ref())
        .and_then(|policy| policy.network.as_deref())
        .is_some_and(|network| network == "allow");

    // `[toolchain.rust] targets`: the host is always available; a listed
    // non-host target needs the dist toolchain for that triple before
    // planning (cross-target planning lands with the action-parity wave).
    if let Some(manifest) = &manifest {
        for target in &manifest.toolchain.rust.targets {
            if target != &toolchain.host_triple {
                let version = manifest
                    .toolchain
                    .rust
                    .version
                    .as_deref()
                    .unwrap_or("(system toolchain)");
                return Err(BuildError::Toolchain(ToolchainError::Missing(format!(
                    "toolchain target {target} is not available; run \
                     `tong toolchain fetch rust --version {version} --target {target}` \
                     and pin `[toolchain.rust] version` (cross-target planning is \
                     not supported yet)"
                ))));
            }
        }
    }

    // Plan.
    let state = tong_store::StateStore::open(&store)?;
    let project_hash = tong_store::project_hash(root).ok();
    let mut backend = RustBackend::with_tests_state(
        cas.clone(),
        &model,
        toolchain,
        &options.profile,
        tests_enabled,
        test_args,
        Some(state),
        project_hash,
        network_allow,
    )?;
    let planned = backend.plan()?;
    let artifacts = backend.final_artifacts();
    let source_trees = backend.captured_source_trees();

    let order = match topological_order(&planned) {
        Ok(order) => order,
        Err(cycle) => {
            return Err(BuildError::Cycle(format!(
                "action cycle: {:?}",
                cycle.remaining
            )));
        }
    };
    let order: Vec<usize> = order
        .iter()
        .map(|action| {
            planned
                .iter()
                .position(|candidate| std::ptr::eq(candidate, *action))
                .expect("topological order references planned actions")
        })
        .collect();
    tracing::debug!(
        target: "tong::perf",
        phase = "prepare.plan",
        actions = planned.len(),
        duration_ms = t_prep.elapsed().as_millis() as u64,
    );

    // Transient git checkouts: every package tree was captured into the
    // CAS during plan(); drop the working trees (PLAN: the importer must
    // not retain extracted checkout directories after their tree is
    // captured).
    for checkout in sources.take_materialized() {
        let _ = fs::remove_dir_all(checkout);
    }

    Ok(Prepared {
        tong_dir,
        store,
        manifest,
        cas,
        cache,
        executor,
        planned,
        artifacts,
        source_trees,
        order,
    })
}

/// Records the build-state manifest and runs the automatic GC (best-effort:
/// failures only warn — cache correctness is unaffected).
///
/// A `--deps-only` build records only dependency actions; it invalidates
/// nothing, so its manifest merges the previous manifest's object closure
/// — the merged manifest stays the GC root and keeps every object the
/// current workspace references (local results included), instead of
/// letting a narrow deps-only manifest orphan the local cache.
#[allow(clippy::too_many_arguments)]
fn record_state(
    root: &Path,
    prepared: &Prepared,
    recorded: &[tong_store::RecordedAction],
    graph_pairs: &BTreeMap<String, tong_core::digest::Digest>,
    sources: &[tong_core::digest::Digest],
    toolchains: &[tong_core::digest::Digest],
    artifact_pairs: &[(String, TreeDigest)],
    profile: &str,
    deps_only: bool,
) -> Result<(), BuildError> {
    if let Ok(project_hash) = project_hash(root) {
        let state = StateStore::open(&prepared.store)?;
        let mut build_manifest = BuildManifest {
            schema_version: tong_store::BUILD_MANIFEST_SCHEMA_VERSION,
            project_hash,
            created_at_unix_secs: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            graph_digest: graph_digest(graph_pairs),
            profiles: vec![profile.to_owned()],
            sources: sources.to_vec(),
            toolchains: toolchains.to_vec(),
            actions: recorded.to_vec(),
            artifacts: artifact_pairs.to_vec(),
        };
        if deps_only && let Some(previous) = state.latest(&project_hash) {
            // Union with the previous closure (dedup by digest): the
            // deps-only build re-verified nothing local, so the previous
            // graph's objects are still reachable from the current
            // sources and must not be swept.
            for digest in previous.sources {
                if !build_manifest.sources.contains(&digest) {
                    build_manifest.sources.push(digest);
                }
            }
            for digest in previous.toolchains {
                if !build_manifest.toolchains.contains(&digest) {
                    build_manifest.toolchains.push(digest);
                }
            }
            for action in previous.actions {
                if !build_manifest
                    .actions
                    .iter()
                    .any(|recorded| recorded.action_digest == action.action_digest)
                {
                    build_manifest.actions.push(action);
                }
            }
            for (name, tree) in previous.artifacts {
                if !build_manifest.artifacts.iter().any(|(n, _)| n == &name) {
                    build_manifest.artifacts.push((name, tree));
                }
            }
        }
        match state.write(&build_manifest) {
            Ok(()) => {
                let retention = retention_policy(prepared.manifest.as_ref())?;
                let max_size = max_size_policy(prepared.manifest.as_ref())?;
                let report = sweep(
                    &prepared.cas,
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
    Ok(())
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

/// Builds the feature requests for a build: CLI flags apply to the
/// selected workspace packages (selection = `--target` labels or all
/// members); `Tong.toml` target-level `features`/`default_features` apply
/// in native mode.
fn feature_requests(
    model: &tong_rust::RustModel,
    options: &BuildOptions,
    manifest: Option<&Manifest>,
) -> Result<Vec<tong_rust::FeatureRequest>, BuildError> {
    let selected = |name: &str| {
        options.targets.is_empty()
            || options
                .targets
                .iter()
                .any(|target| artifact_name_matches(target, name))
    };
    let mut requests = Vec::new();
    if let Some(manifest) = manifest {
        // Native mode: one package per manifest target (cc_import targets
        // are native imports, not feature-bearing packages).
        for (name, target) in &manifest.target {
            if target.rule == "cc_import" || !selected(name) {
                continue;
            }
            let id = model
                .packages
                .iter()
                .find(|p| p.name == *name)
                .map(|p| p.id.clone())
                .ok_or_else(|| {
                    BuildError::Manifest(format!(
                        "target {name:?} produced no feature-bearing package"
                    ))
                })?;
            let mut features = options.features.features.clone();
            if options.features.all_features {
                features.extend(target.features.keys().cloned());
            }
            let default_features = !options.features.no_default_features;
            requests.push(tong_rust::FeatureRequest {
                package: id,
                features,
                default_features,
            });
        }
    } else {
        for id in &model.members {
            if !selected(&id.name) {
                continue;
            }
            let mut features = options.features.features.clone();
            let default_features = if options.features.all_features {
                true
            } else {
                !options.features.no_default_features
            };
            if options.features.all_features
                && let Some(package) = model.packages.iter().find(|p| p.id == *id)
            {
                features.extend(package.features.keys().cloned());
            }
            requests.push(tong_rust::FeatureRequest {
                package: id.clone(),
                features,
                default_features,
            });
        }
    }
    if requests.is_empty() {
        // No selection matched (or an empty native manifest): fall back to
        // all members so the feature map still covers the graph
        // (`[workspace] default_members` narrows the set when declared).
        let fallback: Vec<&tong_rust::PackageId> = if !model.default_members.is_empty() {
            model.default_members.iter().collect()
        } else {
            model.members.iter().collect()
        };
        for id in fallback {
            requests.push(tong_rust::FeatureRequest {
                package: id.clone(),
                features: Vec::new(),
                default_features: true,
            });
        }
    }
    Ok(requests)
}

/// Loads `Tong.toml` when present (`None` in Cargo-import mode).
pub(crate) fn load_manifest(root: &Path) -> Result<Option<Manifest>, BuildError> {
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
    sources: &dyn tong_rust::LockedSourceProvider,
) -> Result<tong_rust::RustModel, BuildError> {
    if let Some(manifest) = manifest {
        manifest_to_model(manifest, root).map_err(BuildError::Manifest)
    } else if root.join("Cargo.toml").exists() {
        // Target-specific deps need the host triple; a single `rustc -vV`
        // query is far cheaper than the full toolchain capture.
        let host_triple = tong_rust::host_triple()?;
        import_cargo_workspace(root, &host_triple, sources)
            .map_err(|err| BuildError::Manifest(err.to_string()))
    } else {
        Err(BuildError::NoManifest)
    }
}

/// Loads the model without resolving registry dependencies (collecting
/// mode — registry edges stay unresolved, path deps import normally).
/// Used by `tong dockerfile`, which only needs members, path deps, and
/// binary names.
pub(crate) fn load_model_unlocked(
    root: &Path,
    manifest: Option<&Manifest>,
) -> Result<tong_rust::RustModel, BuildError> {
    load_model(root, manifest, &CollectProvider::recording_only())
}

/// Runs a built binary target with the given arguments.
pub fn run(
    root: &Path,
    target: &str,
    args: &[String],
    options: &BuildOptions,
) -> Result<i32, BuildError> {
    let outcome = build(root, options)?;
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

/// The lockfile-backed source provider used by builds and `tong fetch`:
/// resolves registry dep edges to their exact locked packages and
/// materializes checkouts from the source store (never the network).
///
/// Every edge is resolved through the lockfile dependency tuples
/// (`<name> <version> <source>` recorded on the parent's locked entry), so
/// two versions of one crate can never alias.
struct LockfileSource {
    lock: Option<tong_fetch::TongLock>,
    store: PathBuf,
    cas: Cas,
    /// Transient checkouts materialized during this build (removed by the
    /// driver after the source trees are captured).
    materialized: std::cell::RefCell<Vec<PathBuf>>,
}

impl LockfileSource {
    fn new(root: &Path, store: &Path, cas: Cas) -> Self {
        let lock = tong_fetch::TongLock::load(root).ok();
        Self {
            lock,
            store: store.to_path_buf(),
            cas,
            materialized: std::cell::RefCell::new(Vec::new()),
        }
    }

    /// Checkouts materialized during this build; the driver removes them
    /// once the package trees are captured.
    fn take_materialized(&self) -> Vec<PathBuf> {
        std::mem::take(&mut *self.materialized.borrow_mut())
    }

    /// The exact locked package for a registry edge.
    ///
    /// 1. The parent's locked entry (exact name/version/source) names every
    ///    edge as a `(name, version, source)` tuple; the first matching
    ///    tuple pins the package exactly.
    /// 2. Version-1 locks (migrated in memory) carry no dependency strings;
    ///    fall back to requirement matching, which the v1 migration made
    ///    unambiguous.
    ///
    /// `Ok(None)` means the edge is not in the locked graph: an inactive
    /// optional dependency (the lock covers the activated feature graph,
    /// cargo-style).
    fn locked_package(
        &self,
        edge: &tong_rust::RegistryEdge,
    ) -> Result<Option<tong_fetch::LockedPackage>, BuildError> {
        let lock = self.lock.as_ref().ok_or_else(|| {
            BuildError::Offline(format!(
                "registry dependency `{}` requires Tong.lock; run `tong lock`",
                edge.package
            ))
        })?;
        let req = semver::VersionReq::parse(&edge.req).map_err(|err| {
            BuildError::Offline(format!(
                "invalid version requirement {:?} for `{}`: {err}",
                edge.req, edge.package
            ))
        })?;

        // The parent's locked entry names its edges exactly.
        let parent_source = edge.parent.lock_source();
        if let Some(parent_entry) =
            lock.exact(&edge.parent.name, &edge.parent.version, &parent_source)
            && !parent_entry.dependencies.is_empty()
        {
            for dep in &parent_entry.dependencies {
                let (name, version, source) = tong_fetch::LockedPackage::parse_dependency(dep);
                if name != edge.package {
                    continue;
                }
                let Some(package) = lock.packages.iter().find(|package| {
                    package.name == name
                        && package.version.to_string() == version
                        && package.source == source
                }) else {
                    // The tuple names a package that is not locked: stale.
                    return Err(BuildError::Offline(format!(
                        "lockfile out of date: `{}` locks a dependency of `{}` that is \
                         not in Tong.lock; run `tong lock`",
                        edge.parent.name, edge.package
                    )));
                };
                if !req.matches(&package.version) {
                    return Err(BuildError::Offline(format!(
                        "lockfile out of date: `{}` requires {} but Tong.lock has {}; \
                         run `tong lock`",
                        edge.package, edge.req, package.version
                    )));
                }
                return Ok(Some(package.clone()));
            }
            // The parent's locked edges do not include this one: an
            // inactive optional edge, or a stale lock.
            if edge.optional {
                return Ok(None);
            }
            return Err(BuildError::Offline(format!(
                "lockfile out of date: `{}` requires `{}` but the locked graph of `{}` \
                 does not include it; run `tong lock`",
                edge.parent.name, edge.package, edge.parent.name
            )));
        }

        // Version-1 fallback: requirement matching over the lock, which the
        // v1 migration kept unambiguous.
        let candidates: Vec<&tong_fetch::LockedPackage> = lock
            .candidates(&edge.package)
            .filter(|package| req.matches(&package.version))
            .collect();
        match candidates.len() {
            0 => {
                // Optional dependencies missing from the lock are inactive
                // by definition (the lock covers the activated feature
                // graph, cargo-style): skip them instead of erroring. A
                // *mandatory* dep missing from the lock is a stale lock.
                if edge.optional {
                    Ok(None)
                } else {
                    Err(BuildError::Offline(format!(
                        "registry dependency `{}` is not in Tong.lock; run `tong lock`",
                        edge.package
                    )))
                }
            }
            1 => Ok(Some(candidates[0].clone())),
            _ => Err(BuildError::Offline(format!(
                "Tong.lock is ambiguous for package `{}` ({} candidates match {}); \
                 run `tong lock`",
                edge.package,
                candidates.len(),
                edge.req
            ))),
        }
    }
}

impl tong_rust::LockedSourceProvider for LockfileSource {
    fn locked_package(
        &self,
        edge: &tong_rust::RegistryEdge,
    ) -> Result<Option<tong_rust::LockedSource>, tong_rust::CargoImportError> {
        let package = match self.locked_package(edge) {
            Ok(Some(package)) => package,
            Ok(None) => return Ok(None),
            Err(err) => {
                return Err(tong_rust::CargoImportError::Unsupported(err.to_string()));
            }
        };
        if edge.git.is_some() {
            // Git edge: materialize the locked tree from the CAS and
            // verify its digest (never the network).
            let Some(source) = package.source.strip_prefix("git+") else {
                return Err(tong_rust::CargoImportError::Unsupported(format!(
                    "`{} {}` is locked with source {:?}, not a git source",
                    package.name, package.version, package.source
                )));
            };
            let (url, commit) = source.split_once('#').ok_or_else(|| {
                tong_rust::CargoImportError::Unsupported(format!(
                    "git source {source:?} has no locked commit"
                ))
            })?;
            let tree_digest = package.tree_digest.as_deref().ok_or_else(|| {
                tong_rust::CargoImportError::Unsupported(format!(
                    "`{} {}` has no locked source tree; run `tong lock`",
                    package.name, package.version
                ))
            })?;
            let tree_digest = tong_core::digest::Digest::from_hex(tree_digest).map_err(|err| {
                tong_rust::CargoImportError::Unsupported(format!(
                    "invalid tree digest {tree_digest:?}: {err}"
                ))
            })?;
            let source_dir = tong_fetch::materialize_tree(
                &self.cas,
                tong_core::artifact::TreeDigest::new(tree_digest),
                &self.store,
                url,
                commit,
            )
            .map_err(|err| {
                tong_rust::CargoImportError::Unsupported(format!("{err}; run `tong lock`"))
            })?;
            self.materialized.borrow_mut().push(source_dir.clone());
            return Ok(Some(tong_rust::LockedSource {
                id: tong_rust::model::PackageId {
                    name: package.name,
                    version: package.version,
                    source: tong_rust::model::SourceId::Git {
                        url: url.to_owned(),
                        rev: commit.to_owned(),
                    },
                },
                source_dir,
                propagate_source: true,
            }));
        }
        let checksum = package.checksum.as_deref().ok_or_else(|| {
            tong_rust::CargoImportError::Unsupported(format!(
                "`{} {}` is not a registry package",
                package.name, package.version
            ))
        })?;
        let source_dir =
            tong_fetch::materialize_source(&self.store, &package.name, &package.version, checksum)
                .map_err(|err| {
                    tong_rust::CargoImportError::Unsupported(format!("{err}; run `tong fetch`"))
                })?;
        let source = tong_rust::model::SourceId::parse_lock_source(&package.source)
            .map_err(tong_rust::CargoImportError::Unsupported)?;
        Ok(Some(tong_rust::LockedSource {
            id: tong_rust::model::PackageId {
                name: package.name,
                version: package.version,
                source,
            },
            source_dir,
            propagate_source: false,
        }))
    }
}

/// Collecting provider used by `tong lock`: records registry edges for the
/// version resolver instead of resolving them. Git edges ARE resolved when
/// `resolve_git` is set (the lock must pin their commits, capture their
/// trees, and import their packages so feature resolution and the
/// resolution graph see them); the dockerfile path records them like
/// registry edges.
struct CollectProvider {
    edges: std::cell::RefCell<Vec<tong_rust::RegistryEdge>>,
    store: Option<PathBuf>,
    cas: Option<Cas>,
    offline: bool,
    /// The existing lock (preferences for git commits).
    preferences: Option<tong_fetch::TongLock>,
    /// `tong update <package>`: drop that package's locked git commit.
    drop_preference: Option<String>,
    /// git source string → captured tree digest hex (lock assembly).
    git_trees: std::cell::RefCell<BTreeMap<String, String>>,
    /// Transient git checkouts created during this lock run.
    git_checkouts: std::cell::RefCell<Vec<PathBuf>>,
    /// Resolve git edges (lock mode); `false` records them like registry
    /// edges (dockerfile mode).
    resolve_git: bool,
}

impl CollectProvider {
    fn for_lock(
        store: PathBuf,
        cas: Cas,
        offline: bool,
        preferences: Option<tong_fetch::TongLock>,
        drop_preference: Option<String>,
    ) -> Self {
        Self {
            edges: std::cell::RefCell::new(Vec::new()),
            store: Some(store),
            cas: Some(cas),
            offline,
            preferences,
            drop_preference,
            git_trees: std::cell::RefCell::new(BTreeMap::new()),
            git_checkouts: std::cell::RefCell::new(Vec::new()),
            resolve_git: true,
        }
    }

    /// Records every edge without resolving anything (dockerfile mode).
    fn recording_only() -> Self {
        Self {
            edges: std::cell::RefCell::new(Vec::new()),
            store: None,
            cas: None,
            offline: false,
            preferences: None,
            drop_preference: None,
            git_trees: std::cell::RefCell::new(BTreeMap::new()),
            git_checkouts: std::cell::RefCell::new(Vec::new()),
            resolve_git: false,
        }
    }

    fn take_edges(&self) -> Vec<tong_rust::RegistryEdge> {
        std::mem::take(&mut *self.edges.borrow_mut())
    }

    fn take_git_trees(&self) -> BTreeMap<String, String> {
        std::mem::take(&mut *self.git_trees.borrow_mut())
    }

    fn take_git_checkouts(&self) -> Vec<PathBuf> {
        std::mem::take(&mut *self.git_checkouts.borrow_mut())
    }

    /// The locked git commit for a package from `url`, when the existing
    /// lock pins one.
    fn locked_git(&self, name: &str, url: &str) -> Option<tong_fetch::LockedGit> {
        let prefix = format!("git+{url}#");
        let lock = self.preferences.as_ref()?;
        let package = lock
            .candidates(name)
            .find(|p| p.source.starts_with(&prefix))?;
        let commit = package.source.strip_prefix(&prefix)?;
        let tree_digest = package.tree_digest.as_deref()?;
        let tree_digest = tong_core::digest::Digest::from_hex(tree_digest).ok()?;
        Some(tong_fetch::LockedGit {
            commit: commit.to_owned(),
            tree_digest: tong_core::artifact::TreeDigest::new(tree_digest),
        })
    }
}

impl tong_rust::LockedSourceProvider for CollectProvider {
    fn locked_package(
        &self,
        edge: &tong_rust::RegistryEdge,
    ) -> Result<Option<tong_rust::LockedSource>, tong_rust::CargoImportError> {
        // Validate the requirement syntax so `tong lock` fails early.
        semver::VersionReq::parse(&edge.req).map_err(|err| {
            tong_rust::CargoImportError::Unsupported(format!(
                "invalid version requirement {:?} for `{}`: {err}",
                edge.req, edge.package
            ))
        })?;
        let Some(selector) = &edge.git else {
            self.edges.borrow_mut().push(edge.clone());
            return Ok(None);
        };
        if !self.resolve_git {
            // Dockerfile mode: the git edge is recorded like a registry
            // edge; nothing is fetched or imported.
            self.edges.borrow_mut().push(edge.clone());
            return Ok(None);
        }
        // Git edge: resolve the selector now (network allowed at lock
        // time), capture the tree, and import the checkout.
        let (store, cas) = (
            self.store.as_ref().expect("lock provider has a store"),
            self.cas.as_ref().expect("lock provider has a cas"),
        );
        let prefer_locked = self.drop_preference.as_deref() != Some(edge.package.as_str());
        let locked = self.locked_git(&edge.package, &selector.url);
        if self.offline && prefer_locked && locked.is_none() {
            return Err(tong_rust::CargoImportError::Unsupported(format!(
                "git dependency `{}` from {} is not locked; run `tong lock` online once \
                 (--offline forbids fetching repositories)",
                edge.package, selector.url
            )));
        }
        if self.offline && !prefer_locked {
            return Err(tong_rust::CargoImportError::Unsupported(format!(
                "cannot re-resolve git dependency `{}` from {} while offline; \
                 run `tong update` online",
                edge.package, selector.url
            )));
        }
        let resolved = tong_fetch::resolve_and_capture(
            store,
            cas,
            &selector.url,
            selector.rev.as_deref(),
            selector.tag.as_deref(),
            selector.branch.as_deref(),
            prefer_locked,
            locked.as_ref(),
        )
        .map_err(|err| {
            tong_rust::CargoImportError::Unsupported(format!(
                "git dependency `{}` from {}: {err}",
                edge.package, selector.url
            ))
        })?;
        let source = tong_rust::SourceId::Git {
            url: selector.url.clone(),
            rev: resolved.commit.clone(),
        };
        self.git_trees
            .borrow_mut()
            .insert(source.lock_source(), resolved.tree_digest.digest().to_hex());
        self.git_checkouts
            .borrow_mut()
            .push(resolved.checkout.clone());
        Ok(Some(tong_rust::LockedSource {
            id: tong_rust::PackageId {
                name: edge.package.clone(),
                version: semver::Version::new(0, 0, 0),
                source,
            },
            source_dir: resolved.checkout,
            propagate_source: true,
        }))
    }
}

/// The registry configuration: env `TONG_REGISTRY_INDEX` → `[registry]
/// index` → crates.io.
fn registry_config(manifest: Option<&Manifest>) -> Result<tong_fetch::RegistryConfig, BuildError> {
    let index = std::env::var("TONG_REGISTRY_INDEX")
        .ok()
        .or_else(|| {
            manifest
                .and_then(|manifest| manifest.registry.as_ref())
                .and_then(|registry| registry.index.clone())
        })
        .unwrap_or_else(|| "sparse+https://index.crates.io/".to_owned());
    tong_fetch::RegistryConfig::from_url(&index).map_err(|err| BuildError::Store(err.to_string()))
}

/// Writes `Tong.lock`: imports the manifests (collecting registry edges),
/// resolves versions against the index (using the existing lock as
/// preference + yanked allowance), and records registry and path packages.
pub fn lock(root: &Path, offline: bool) -> Result<(), BuildError> {
    lock_with(root, offline, None)
}

/// Re-resolves `Tong.lock`; `package` drops only that package's lockfile
/// preference (Cargo `update -p` semantics).
pub fn update(root: &Path, package: Option<&str>) -> Result<(), BuildError> {
    lock_with(root, false, package)
}

/// Seeds a resolver preference from an existing `Cargo.lock` (no
/// `Tong.lock` yet): exact registry versions and checksums carry over, so
/// `tong lock` is stable against a Cargo-generated lock. Registry entries
/// are matched by name+version; the Cargo source strings are remapped to
/// the configured index. Path/git entries are not seeded (the workspace
/// import resolves those).
fn seed_from_cargo_lock(
    root: &Path,
    registry_source: &str,
) -> Result<tong_fetch::TongLock, BuildError> {
    #[derive(serde::Deserialize)]
    struct CargoLock {
        #[serde(default)]
        package: Vec<CargoLockPackage>,
    }
    #[derive(serde::Deserialize)]
    struct CargoLockPackage {
        name: String,
        version: semver::Version,
        source: Option<String>,
        checksum: Option<String>,
    }
    let text = fs::read_to_string(root.join("Cargo.lock"))
        .map_err(|err| BuildError::Manifest(format!("cannot read Cargo.lock: {err}")))?;
    let cargo: CargoLock = toml::from_str(&text)
        .map_err(|err| BuildError::Manifest(format!("cannot parse Cargo.lock: {err}")))?;
    let mut lock = tong_fetch::TongLock {
        version: tong_fetch::LOCKFILE_VERSION,
        packages: Vec::new(),
    };
    for package in cargo.package {
        if package
            .source
            .as_deref()
            .is_some_and(|source| source.starts_with("registry+"))
        {
            lock.packages.push(tong_fetch::LockedPackage {
                name: package.name,
                version: package.version,
                source: registry_source.to_owned(),
                checksum: package.checksum,
                yanked: false,
                publish_time: None,
                manifest_checksum: None,
                tree_digest: None,
                dependencies: Vec::new(),
            });
        }
    }
    Ok(lock)
}

fn lock_with(root: &Path, offline: bool, drop_preference: Option<&str>) -> Result<(), BuildError> {
    let manifest = load_manifest(root)?;
    let store = store_dir(root, manifest.as_ref())?;
    let cas = Cas::open(&store)?;

    // Index client (cached under <store>/index/).
    let registry = registry_config(manifest.as_ref())?;
    let mut index = tong_fetch::IndexClient::new(store.join("index"), registry.clone());
    index.set_offline(offline);

    // Preferences: the existing lock pins registry versions and git
    // commits. With no Tong.lock but a Cargo.lock, seed exact registry
    // versions/checksums (Cargo semantics: `--locked` forbids creating or
    // updating the lock — enforced by the build driver before any network
    // request). `tong update <package>` drops that package's preference.
    let mut preferences = tong_fetch::TongLock::load(root).unwrap_or_default();
    if preferences.packages.is_empty() && root.join("Cargo.lock").is_file() {
        let registry_source = format!("registry+{}", registry.index_url);
        preferences = seed_from_cargo_lock(root, &registry_source)?;
    }
    if let Some(package) = drop_preference {
        preferences.packages.retain(|p| p.name != package);
    }

    // Collect registry edges via a collecting provider; git edges are
    // resolved by the provider itself (commits pinned against the
    // preferences, trees captured into the CAS, checkouts imported) so the
    // git packages join the normal model and feature graph.
    let provider = CollectProvider::for_lock(
        store.clone(),
        cas.clone(),
        offline,
        Some(preferences.clone()),
        drop_preference.map(str::to_owned),
    );
    let model = load_model(root, manifest.as_ref(), &provider)?;
    let edges = provider.take_edges();
    let git_trees = provider.take_git_trees();
    let git_checkouts = provider.take_git_checkouts();

    // Feature resolution decides which optional edges are live (git
    // packages are in the model now, so their optional deps gate too).
    let requests = feature_requests(&model, &BuildOptions::default(), manifest.as_ref())?;
    let feature_map = tong_rust::resolve_features(&model, &requests, true)
        .map_err(|err| BuildError::Manifest(err.to_string()))?;

    // The version resolver keys local (workspace/path) packages by name.
    // Two local packages sharing a name would alias there; Cargo can
    // express that (path deps at different directories), but Tong's
    // resolver is name-keyed — reject with a targeted diagnostic instead
    // of silently picking one. Registry packages never enter `locals`, so
    // a local `foo` and a registry `foo` coexist fine.
    let mut local_names: BTreeMap<&str, &tong_rust::Package> = BTreeMap::new();
    for pkg in &model.packages {
        if let Some(previous) = local_names.insert(&pkg.name, pkg) {
            return Err(BuildError::Manifest(format!(
                "two local packages named {} ({} and {}); Tong cannot resolve \
                 same-name path packages yet — rename one or use \
                 `package = \"...\"` renames",
                pkg.name,
                previous.dir.display(),
                pkg.dir.display()
            )));
        }
    }

    // Workspace/path packages enter the resolution graph as roots; their
    // registry edges resolve against the index, local edges activate the
    // target package at its exact version (cargo semantics). Optional
    // edges are filtered by the resolved feature map, exactly like the
    // roots were.
    let edge_map: BTreeMap<(tong_rust::PackageId, String, String), &tong_rust::RegistryEdge> =
        edges
            .iter()
            .map(|edge| {
                (
                    (
                        edge.parent.clone(),
                        edge.package.clone(),
                        edge.extern_name.clone(),
                    ),
                    edge,
                )
            })
            .collect();
    let mut locals: Vec<tong_fetch::LocalPackage> = Vec::new();
    let mut registry_edges = 0usize;
    for pkg in &model.packages {
        // Local (path) edges come from the model; registry edges come from
        // the collected edge list — collecting mode deliberately drops
        // registry deps from the model's dep lists (the provider records
        // them instead).
        let active = |extern_name: &str, optional: bool| {
            !optional || feature_map.edge_active(&pkg.id, extern_name)
        };
        let mut deps: Vec<tong_fetch::ResolvedDep> = Vec::new();
        for (dep, dev) in pkg
            .deps
            .iter()
            .map(|dep| (dep, false))
            .chain(pkg.build_deps.iter().map(|dep| (dep, false)))
            .chain(pkg.dev_deps.iter().map(|dep| (dep, true)))
        {
            let is_registry = edge_map.contains_key(&(
                pkg.id.clone(),
                dep.package.name.clone(),
                dep.extern_name.clone(),
            ));
            if is_registry || !active(&dep.extern_name, dep.optional) {
                continue;
            }
            deps.push(tong_fetch::ResolvedDep {
                name: dep.package.name.clone(),
                req: None,
                optional: dep.optional,
                dev,
                features: dep.features.clone(),
                default_features: dep.default_features,
            });
        }
        let dev_edges: BTreeSet<(String, String)> = pkg
            .dev_deps
            .iter()
            .map(|dep| (dep.package.name.clone(), dep.extern_name.clone()))
            .collect();
        for edge in edges.iter().filter(|edge| edge.parent == pkg.id) {
            if !active(&edge.extern_name, edge.optional) {
                continue;
            }
            let req = semver::VersionReq::parse(&edge.req)
                .map_err(|err| BuildError::Manifest(err.to_string()))?;
            let dev = dev_edges.contains(&(edge.package.clone(), edge.extern_name.clone()));
            deps.push(tong_fetch::ResolvedDep {
                name: edge.package.clone(),
                req: Some(req),
                optional: edge.optional,
                dev,
                features: edge.features.clone(),
                default_features: edge.default_features,
            });
            registry_edges += 1;
        }
        deps.sort_by(|a, b| a.name.cmp(&b.name));
        locals.push(tong_fetch::LocalPackage {
            name: pkg.name.clone(),
            version: semver::Version::parse(&pkg.version)
                .unwrap_or_else(|_| semver::Version::new(0, 0, 0)),
            source: Some(pkg.id.lock_source()),
            deps,
        });
    }
    println!(
        "resolving {} packages ({} registry edges) against {}",
        locals.len(),
        registry_edges,
        registry.index_url
    );
    let t_resolve = std::time::Instant::now();
    let resolved = tong_fetch::resolve(&index, &locals, &preferences)
        .map_err(|err| BuildError::Manifest(err.to_string()))?;
    tracing::info!(
        target: "tong::lock",
        phase = "lock.resolve",
        packages = resolved.len(),
        duration_ms = t_resolve.elapsed().as_millis() as u64,
    );

    // Assemble the lock: every resolved package, registry or local, with
    // exact per-edge identities (a name may resolve to several versions or
    // sources). Sources come from the resolver — a local `foo` and a
    // registry `foo` never alias.
    let mut locked = tong_fetch::TongLock {
        version: tong_fetch::LOCKFILE_VERSION,
        packages: Vec::new(),
    };
    let registry_source = format!("registry+{}", registry.index_url);
    // Local packages are unique by name (checked above); find the dir for
    // the manifest checksum.
    let local_pkg = |name: &str| {
        model.packages.iter().find(|p| {
            p.id.name == name
                && matches!(
                    p.id.source,
                    tong_rust::model::SourceId::Workspace(_)
                        | tong_rust::model::SourceId::Path(_)
                        | tong_rust::model::SourceId::Git { .. }
                )
        })
    };
    for package in &resolved {
        let local = if package.local {
            local_pkg(&package.name)
        } else {
            None
        };
        let source = package
            .source
            .clone()
            .unwrap_or_else(|| registry_source.clone());
        let manifest_checksum = if let Some(pkg) = local {
            fs::read(pkg.dir.join("Cargo.toml"))
                .ok()
                .map(|bytes| tong_core::digest::Hasher::digest(&bytes).to_hex())
        } else {
            None
        };
        // Git packages carry the canonical source-tree digest captured at
        // lock time.
        let tree_digest = source
            .strip_prefix("git+")
            .and_then(|_| git_trees.get(&source).cloned());
        let mut deps: Vec<String> = package
            .dependencies
            .iter()
            .map(|(name, version, dep_source)| {
                let dep_source = dep_source
                    .clone()
                    .unwrap_or_else(|| registry_source.clone());
                format!("{name} {version} {dep_source}")
            })
            .collect();
        deps.sort();
        deps.dedup();
        locked.packages.push(tong_fetch::LockedPackage {
            name: package.name.clone(),
            version: package.version.clone(),
            source,
            checksum: package.checksum.clone(),
            manifest_checksum,
            tree_digest,
            yanked: package.yanked,
            publish_time: None,
            dependencies: deps,
        });
    }
    locked
        .save(root)
        .map_err(|err| BuildError::Manifest(err.to_string()))?;
    // Git checkouts are transient at lock time: their content lives in
    // the CAS (the tree digests just recorded); drop the working trees.
    for checkout in git_checkouts {
        let _ = fs::remove_dir_all(checkout);
    }
    println!("wrote Tong.lock ({} packages)", locked.packages.len());
    Ok(())
}

/// Downloads a pinned dist toolchain into the store.
pub fn toolchain_fetch(root: &Path, version: &str, target: Option<&str>) -> Result<(), BuildError> {
    let manifest = load_manifest(root)?;
    let store = store_dir(root, manifest.as_ref())?;
    let cas = Cas::open(&store)?;
    let host_triple = tong_rust::host_triple()?;
    let triple = target.unwrap_or(&host_triple);
    let toolchain = tong_rust::fetch_dist_rust(&cas, &store, version, triple, &host_triple)
        .map_err(BuildError::Toolchain)?;
    let _ = toolchain;
    println!("fetched rust {version} ({triple})");
    Ok(())
}

/// Downloads every locked registry package into the source store; a no-op
/// when everything is already stored. `--offline` never touches the
/// network: missing blobs fail with a targeted diagnostic before any
/// download is attempted.
pub fn fetch(root: &Path, offline: bool) -> Result<(), BuildError> {
    let manifest = load_manifest(root)?;
    if !root.join("Tong.lock").is_file() {
        if offline {
            return Err(BuildError::Offline(
                "no Tong.lock; run `tong lock` first \
                 (--offline forbids auto-locking)"
                    .to_owned(),
            ));
        }
        println!("tong: no Tong.lock — running `tong lock` first");
        lock(root, false)?;
    }
    let store = store_dir(root, manifest.as_ref())?;
    let cas = Cas::open(&store)?;
    let lock =
        tong_fetch::TongLock::load(root).map_err(|err| BuildError::Manifest(err.to_string()))?;
    let mut registry = registry_config(manifest.as_ref())?;
    let missing: Vec<&tong_fetch::LockedPackage> = lock
        .packages
        .iter()
        .filter(|package| {
            if package.source.starts_with("registry+") {
                package.checksum.as_deref().is_some_and(|checksum| {
                    !tong_fetch::crate_blob_path(&store, checksum).is_file()
                })
            } else if package.source.starts_with("git+") {
                // The locked source tree must be present in the CAS.
                package.tree_digest.as_deref().is_some_and(|tree| {
                    tong_core::digest::Digest::from_hex(tree)
                        .ok()
                        .and_then(|digest| cas.get_tree(TreeDigest::new(digest)).ok().flatten())
                        .is_none()
                })
            } else {
                false
            }
        })
        .collect();
    if offline && !missing.is_empty() {
        return Err(BuildError::Offline(format!(
            "sources not fetched: missing {}; run `tong fetch` online first",
            missing
                .iter()
                .map(|package| format!("{} {}", package.name, package.version))
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    let total = lock
        .packages
        .iter()
        .filter(|package| {
            (package.source.starts_with("registry+") && package.checksum.is_some())
                || package.source.starts_with("git+")
        })
        .count();
    let mut fetched = 0usize;
    for package in &lock.packages {
        if package.source.starts_with("registry+") {
            let Some(checksum) = &package.checksum else {
                continue;
            };
            println!(
                "  downloading {}/{} {} {}",
                fetched + 1,
                total,
                package.name,
                package.version
            );
            let resolved = tong_fetch::ResolvedPackage {
                name: package.name.clone(),
                version: package.version.clone(),
                source: None,
                checksum: Some(checksum.clone()),
                yanked: package.yanked,
                local: false,
                dependencies: Vec::new(),
            };
            tong_fetch::fetch_crate(&cas, &mut registry, &resolved)
                .map_err(|err| BuildError::Manifest(err.to_string()))?;
            fetched += 1;
        } else if let Some(rest) = package.source.strip_prefix("git+") {
            // Materialize the locked tree from the CAS and verify it.
            let Some((url, commit)) = rest.split_once('#') else {
                continue;
            };
            let tree_digest = package.tree_digest.as_deref().ok_or_else(|| {
                BuildError::Manifest(format!(
                    "`{} {}` has no locked source tree; run `tong lock`",
                    package.name, package.version
                ))
            })?;
            let tree_digest = tong_core::digest::Digest::from_hex(tree_digest)
                .map_err(|err| BuildError::Manifest(format!("invalid tree digest: {err}")))?;
            println!(
                "  materializing {}/{} {} {}",
                fetched + 1,
                total,
                package.name,
                package.version
            );
            tong_fetch::materialize_tree(&cas, TreeDigest::new(tree_digest), &store, url, commit)
                .map_err(|err| BuildError::Manifest(format!("{err}; run `tong lock`")))?;
            fetched += 1;
        }
    }
    println!("fetched {fetched} crates");
    Ok(())
}

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
