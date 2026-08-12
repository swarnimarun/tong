//! The build driver: manifests → toolchain → plan → schedule → execute →
//! assemble (PLAN.md section 15, Phase 1 pipeline).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

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
    ActionCache, BuildManifest, CachedResult, Cas, ClosureVerifier, GcOptions, GcReport,
    StateStore, graph_digest, project_hash, sweep,
};

use crate::manifest_mode::manifest_to_model;

/// Build-target kinds for kind selectors (`--lib`/`--bins`/`--tests`/…).
pub const KIND_LIB: u32 = 1 << 0;
pub const KIND_BIN: u32 = 1 << 1;
pub const KIND_TEST: u32 = 1 << 2;
pub const KIND_EXAMPLE: u32 = 1 << 3;
pub const KIND_BENCH: u32 = 1 << 4;
/// Every target kind.
pub const KIND_ALL: u32 = KIND_LIB | KIND_BIN | KIND_TEST | KIND_EXAMPLE | KIND_BENCH;

/// One Cargo-style singular target selector (`--bin`, `--example`,
/// `--test`, or `--bench`).
#[derive(Clone, Debug)]
pub struct TargetSelection {
    pub kind: &'static str,
    pub name: String,
}

/// Default retention for unmarked cache objects (auto-GC after builds).
pub const DEFAULT_RETENTION: &str = "7d";
/// Default store size budget (auto-GC after builds).
pub const DEFAULT_MAX_SIZE: &str = "10G";

/// Process-wide store override set by the CLI's global `--store-dir` flag.
///
/// The driver is otherwise configured from manifests and environment variables;
/// keeping this override outside action configuration ensures that a physical
/// cache location never changes semantic action identity.
static STORE_DIR_OVERRIDE: OnceLock<PathBuf> = OnceLock::new();

static OUTPUT_OPTIONS: OnceLock<OutputOptions> = OnceLock::new();

#[derive(Clone, Copy, Debug, Default, clap::ValueEnum)]
pub enum MessageFormat {
    #[default]
    Human,
    Json,
}

#[derive(Clone, Copy, Debug, Default)]
struct OutputOptions {
    verbose: bool,
    message_format: MessageFormat,
}

pub fn set_output_options(verbose: bool, message_format: MessageFormat) {
    let _ = OUTPUT_OPTIONS.set(OutputOptions {
        verbose,
        message_format,
    });
}

pub fn json_output() -> bool {
    matches!(output_options().message_format, MessageFormat::Json)
}

fn output_options() -> OutputOptions {
    OUTPUT_OPTIONS.get().copied().unwrap_or_default()
}

struct Progress {
    started: std::time::Instant,
    options: OutputOptions,
}

impl Progress {
    fn new() -> Self {
        Self {
            started: std::time::Instant::now(),
            options: output_options(),
        }
    }

    fn event(&self, kind: &str, fields: serde_json::Value) {
        if matches!(self.options.message_format, MessageFormat::Json) {
            let mut event = serde_json::json!({
                "schema_version": 1,
                "type": kind,
                "elapsed_ms": self.started.elapsed().as_millis() as u64,
            });
            if let (Some(event), Some(fields)) = (event.as_object_mut(), fields.as_object()) {
                event.extend(fields.clone());
            }
            println!("{event}");
        }
    }

    fn phase_started(&self, phase: &str) {
        self.event("phase-started", serde_json::json!({ "phase": phase }));
    }

    fn phase_finished(&self, phase: &str, duration: std::time::Duration, detail: &str) {
        self.event(
            "phase-finished",
            serde_json::json!({
                "phase": phase,
                "duration_ms": duration.as_millis() as u64,
            }),
        );
        if matches!(self.options.message_format, MessageFormat::Human) {
            eprintln!("{phase} {detail} {}", display_duration(duration));
        }
    }

    fn action_started(&self, action: &str, mnemonic: &str) {
        self.event(
            "action-started",
            serde_json::json!({ "action": action, "mnemonic": mnemonic }),
        );
    }

    fn action_finished(&self, action: &str, cached: bool, duration: std::time::Duration) {
        self.event(
            "action-finished",
            serde_json::json!({
                "action": action,
                "cached": cached,
                "duration_ms": duration.as_millis() as u64,
                "outcome": "success",
            }),
        );
    }

    fn command_finished(
        &self,
        command: &str,
        duration: std::time::Duration,
        fields: serde_json::Value,
    ) {
        let mut detail = serde_json::json!({
            "command": command,
            "duration_ms": duration.as_millis() as u64,
            "outcome": "success",
        });
        if let (Some(detail), Some(fields)) = (detail.as_object_mut(), fields.as_object()) {
            detail.extend(fields.clone());
        }
        self.event("command-finished", detail);
    }
}

fn display_duration(duration: std::time::Duration) -> String {
    if duration.as_millis() < 1_000 {
        format!("{}ms", duration.as_millis())
    } else {
        format!("{:.2}s", duration.as_secs_f64())
    }
}

/// Returns path/workspace packages whose current Cargo manifest no longer
/// matches the fingerprint recorded in `Tong.lock`. This is deliberately an
/// "appears stale" check: it catches changed and removed known manifests,
/// while full dependency-graph validation still happens during analysis.
fn stale_lockfile_packages(root: &Path, lock: &tong_fetch::TongLock) -> Vec<String> {
    lock.packages
        .iter()
        .filter_map(|package| {
            let rel = package.source.strip_prefix("path+")?;
            let expected = package.manifest_checksum.as_deref()?;
            let manifest = root.join(rel).join("Cargo.toml");
            let current = fs::read(&manifest)
                .ok()
                .map(|bytes| tong_core::digest::Hasher::digest(&bytes).to_hex());
            (current.as_deref() != Some(expected)).then(|| package.name.clone())
        })
        .collect()
}

fn stale_lockfile_message(packages: &[String]) -> String {
    format!(
        "Tong.lock appears stale (changed manifests: {}); run `tong lock`",
        packages.join(", ")
    )
}

fn report_stale_lockfile(progress: &Progress, packages: &[String]) {
    if packages.is_empty() {
        return;
    }
    progress.event(
        "lockfile-stale",
        serde_json::json!({
            "packages": packages,
            "remedy": "run `tong lock`",
        }),
    );
    if matches!(output_options().message_format, MessageFormat::Human) {
        eprintln!("tong: warning: {}", stale_lockfile_message(packages));
    }
}

/// Sets the process-wide store directory selected by the CLI.
pub fn set_store_dir_override(dir: PathBuf) {
    let _ = STORE_DIR_OVERRIDE.set(dir);
}

/// Resolves the store directory for a workspace.
///
/// Resolution order: CLI `--store-dir` → env `TONG_STORE_DIR` → `[store] dir`
/// in `Tong.toml` (relative to the workspace root; native mode only) →
/// `<root>/.tong/store` (the project-local default). Everything else (exec
/// roots, `out/`) stays under `<root>/.tong/` in both modes.
pub fn store_dir(root: &Path, manifest: Option<&Manifest>) -> Result<PathBuf, BuildError> {
    if let Some(dir) = STORE_DIR_OVERRIDE.get() {
        return Ok(dir.clone());
    }
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

/// Resolves the effective store path for a workspace without opening it.
pub fn resolved_store_dir(root: &Path) -> Result<PathBuf, BuildError> {
    let manifest = load_manifest(root)?;
    store_dir(root, manifest.as_ref())
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
    /// Select every workspace member instead of `default-members`.
    pub workspace: bool,
    /// Workspace package specs excluded from the selected roots.
    pub excludes: Vec<String>,
    /// Singular target selectors, kept separate from package selection.
    pub target_selections: Vec<TargetSelection>,
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
    /// `--check`: rustc emits metadata only; nothing is assembled.
    pub check: bool,
    /// `--no-run`: test/bench compiles without their run actions.
    pub no_run: bool,
    /// Target kinds to build (`KIND_*` bitmask).
    pub kinds: u32,
    /// Rust target triple for target units (`--target`; host by default).
    pub target_triple: Option<String>,
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
    let _workspace_lock = WorkspaceBuildLock::acquire(root)?;
    let progress = Progress::new();
    progress.phase_started("Preparing");
    // Test/bench/example targets pull in dev-dependencies (cargo's
    // `--all-targets` semantics); plain builds exclude them.
    let include_dev = options.kinds & (KIND_TEST | KIND_EXAMPLE | KIND_BENCH) != 0;
    let prepared = prepare(root, options, include_dev, &[])?;
    let prepare_duration = t_build.elapsed();
    progress.phase_finished(
        "Preparing",
        prepare_duration,
        &format!("{} actions", prepared.order.len()),
    );
    let tong_dir = &prepared.tong_dir;
    let cas = &prepared.cas;
    let cache = &prepared.cache;
    let executor = &prepared.executor;
    let order = &prepared.order;

    // Schedule: concretize, check cache, execute.
    let mut completed = CompletedMap(BTreeMap::new());
    let mut recorded: Vec<tong_store::RecordedAction> = Vec::new();
    let mut events: Vec<BuildEvent> = Vec::new();
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
    let mut closure_verifier = ClosureVerifier::default();
    let mut concretize_duration = std::time::Duration::ZERO;
    let mut result_lookup_duration = std::time::Duration::ZERO;
    let mut closure_verify_duration = std::time::Duration::ZERO;
    let mut execute_duration = std::time::Duration::ZERO;
    let mut cache_checked = 0usize;
    progress.phase_started("Checking cache");
    progress.phase_started("Executing");

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
        let t_concretize = std::time::Instant::now();
        let spec = (action.make)(&completed, cas)?;
        concretize_duration += t_concretize.elapsed();
        let digest = spec.digest();
        // `CachePolicy::NoCache` actions (test runs, network-allowed
        // actions) must bypass the action cache entirely: no lookup, no
        // insertion. Their digest still covers the policy, so a NoCache
        // action can never alias a cacheable one.
        let cacheable = spec.cache_policy == CachePolicy::Enabled;
        let t_action = std::time::Instant::now();
        progress.action_started(&spec.logical_id.0, &spec.mnemonic);
        let mut cache_source = "executed";
        let mut cached_result = if cacheable {
            cache_checked += 1;
            let t_lookup = std::time::Instant::now();
            let lookup = cache.get(digest)?;
            result_lookup_duration += t_lookup.elapsed();
            match lookup {
                Some(result) => {
                    let t_verify = std::time::Instant::now();
                    let complete = result.is_complete_cached(cas, &mut closure_verifier)?;
                    closure_verify_duration += t_verify.elapsed();
                    if complete {
                        Some(result)
                    } else {
                        cache.remove(digest)?;
                        None
                    }
                }
                None => None,
            }
        } else {
            None
        };
        if cached_result.is_none() && cacheable && action.input_narrowed {
            cached_result = reuse_narrowed_result(&prepared, &spec, digest, &mut closure_verifier)?;
        }
        let cached = if let Some(result) = cached_result {
            outcome.actions_cached += 1;
            cache_source = "cached";
            if output_options().verbose {
                eprintln!(
                    "  [{}/{}] {} ({}) [cached]",
                    index + 1,
                    order.len(),
                    spec.logical_id.0,
                    spec.mnemonic
                );
            }
            progress.action_finished(&spec.logical_id.0, true, t_action.elapsed());
            result
        } else {
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
            execute_duration += result.duration;
            if matches!(output_options().message_format, MessageFormat::Human) {
                eprintln!(
                    "  Executed [{}/{}] {} ({}) {}",
                    index + 1,
                    order.len(),
                    spec.logical_id.0,
                    spec.mnemonic,
                    display_duration(result.duration)
                );
            }
            progress.action_finished(&spec.logical_id.0, false, result.duration);
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
        events.push(BuildEvent {
            action: spec.logical_id.0.clone(),
            digest,
            cache: cache_source,
            duration_ms: t_action.elapsed().as_millis() as u64,
            outcome: "success",
        });
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
    tracing::debug!(
        target: "tong::perf",
        phase = "schedule.detail",
        concretize_ms = concretize_duration.as_millis() as u64,
        result_lookup_ms = result_lookup_duration.as_millis() as u64,
        closure_verify_ms = closure_verify_duration.as_millis() as u64,
    );
    let cache_duration = concretize_duration + result_lookup_duration + closure_verify_duration;
    progress.phase_finished(
        "Checking cache",
        cache_duration,
        &format!(
            "{cache_checked}/{cache_checked} · {} hits · {} misses",
            outcome.actions_cached, outcome.actions_executed
        ),
    );
    progress.phase_finished(
        "Executing",
        execute_duration,
        &format!("{} actions", outcome.actions_executed),
    );
    progress.phase_started("Materializing");
    let t_assemble = std::time::Instant::now();
    let mut artifact_pairs: Vec<(String, TreeDigest)> = Vec::new();
    if !options.deps_only {
        // The configured model has already applied package/label selection,
        // and `final_artifacts` has already applied singular target selectors.
        // Filtering again by the raw package spec is incorrect: a native
        // target key or Cargo package name need not equal its binary output
        // name (`//app:app` may produce `web-app`, `-p server` may produce
        // `daemon`). Materialize exactly the frontend-selected artifacts.
        for artifact in &prepared.artifacts {
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
    progress.phase_finished(
        "Materializing",
        t_assemble.elapsed(),
        &format!("{} artifacts", artifact_pairs.len()),
    );
    let t_record = std::time::Instant::now();
    progress.phase_started("Finishing");

    let state_changed = record_state(
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
    record_events(root, &prepared.store, &events);
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
    progress.phase_finished(
        "Finishing",
        t_record.elapsed(),
        if state_changed {
            "state updated"
        } else {
            "state unchanged"
        },
    );
    let t_cleanup = std::time::Instant::now();
    drop(closure_verifier);
    drop(prepared);
    drop(_workspace_lock);
    tracing::debug!(
        target: "tong::perf",
        phase = "cleanup",
        duration_ms = t_cleanup.elapsed().as_millis() as u64,
    );
    progress.event(
        "build-finished",
        serde_json::json!({
            "profile": options.profile,
            "actions": outcome.actions_total,
            "cached": outcome.actions_cached,
            "executed": outcome.actions_executed,
            "skipped": outcome.actions_skipped,
            "duration_ms": t_build.elapsed().as_millis() as u64,
            "outcome": "success",
        }),
    );
    if matches!(output_options().message_format, MessageFormat::Human) {
        eprintln!(
            "Finished {} · {} cached, {} executed {}",
            options.profile,
            outcome.actions_cached,
            outcome.actions_executed,
            display_duration(t_build.elapsed())
        );
    }

    Ok(outcome)
}

/// Serializes builds that mutate one workspace's `.tong` state. Action-level
/// in-flight deduplication needs scheduler coordination; until that exists,
/// waiting is safer than allowing one build to prune or rewrite files another
/// live build still needs.
struct WorkspaceBuildLock {
    _file: fs::File,
}

impl WorkspaceBuildLock {
    fn acquire(root: &Path) -> io::Result<Self> {
        let tong_dir = root.join(".tong");
        fs::create_dir_all(&tong_dir)?;
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(tong_dir.join("build.lock"))?;
        match file.try_lock() {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => {
                eprintln!("tong: another build is running; waiting for it to finish");
                file.lock()?;
            }
            Err(fs::TryLockError::Error(err)) => return Err(err),
        }
        Ok(Self { _file: file })
    }
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
    let t_total = std::time::Instant::now();
    let _workspace_lock = WorkspaceBuildLock::acquire(root)?;
    let progress = Progress::new();
    progress.phase_started("Preparing");
    let prepared = prepare(root, options, true, libtest_args)?;
    progress.phase_finished(
        "Preparing",
        t_total.elapsed(),
        &format!("{} actions", prepared.order.len()),
    );
    let cas = &prepared.cas;
    let cache = &prepared.cache;
    let executor = &prepared.executor;
    let order = &prepared.order;

    let mut completed = CompletedMap(BTreeMap::new());
    let mut recorded: Vec<tong_store::RecordedAction> = Vec::new();
    let mut events: Vec<BuildEvent> = Vec::new();
    let mut graph_pairs: BTreeMap<String, tong_core::digest::Digest> = BTreeMap::new();
    let mut sources: Vec<tong_core::digest::Digest> = Vec::new();
    let mut toolchains: Vec<tong_core::digest::Digest> = Vec::new();
    // Executed test runs in (label, stdout blob) order.
    let mut test_runs: Vec<(String, tong_core::artifact::BlobDigest)> = Vec::new();
    let mut failed = false;
    let mut closure_verifier = ClosureVerifier::default();
    let mut actions_cached = 0usize;
    let mut actions_executed = 0usize;
    let mut execute_duration = std::time::Duration::ZERO;
    let t_schedule = std::time::Instant::now();
    progress.phase_started("Checking cache");
    progress.phase_started("Executing");

    for index in 0..order.len() {
        let action = &prepared.planned[order[index]];
        let spec = (action.make)(&completed, cas)?;
        let digest = spec.digest();

        // Test runs: skipped when they do not match the label filter.
        let is_test_run = spec.logical_id.0.starts_with("rust:test-run:");
        let t_action = std::time::Instant::now();
        let mut cache_source = "executed";
        if is_test_run
            && let Some(label) = label
            && !test_run_matches(&spec.logical_id.0, label)
        {
            continue;
        }
        progress.action_started(&spec.logical_id.0, &spec.mnemonic);

        // Test runs are `CachePolicy::NoCache`: never look up or insert —
        // every run re-executes (deterministic native tests opt into
        // caching via `cache_test_result = true`).
        let cacheable = spec.cache_policy == CachePolicy::Enabled;
        let mut cached_result = if cacheable {
            match cache.get(digest)? {
                Some(result) if result.is_complete_cached(cas, &mut closure_verifier)? => {
                    Some(result)
                }
                Some(_) => {
                    cache.remove(digest)?;
                    None
                }
                None => None,
            }
        } else {
            None
        };
        if cached_result.is_none() && cacheable && action.input_narrowed {
            cached_result = reuse_narrowed_result(&prepared, &spec, digest, &mut closure_verifier)?;
        }
        let cached = if let Some(result) = cached_result {
            cache_source = "cached";
            actions_cached += 1;
            if output_options().verbose {
                eprintln!(
                    "  [{}/{}] {} ({}) [cached]",
                    index + 1,
                    order.len(),
                    spec.logical_id.0,
                    spec.mnemonic
                );
            }
            progress.action_finished(&spec.logical_id.0, true, t_action.elapsed());
            result
        } else {
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
            actions_executed += 1;
            execute_duration += result.duration;
            if matches!(output_options().message_format, MessageFormat::Human) {
                eprintln!(
                    "  Executed [{}/{}] {} ({}) {}",
                    index + 1,
                    order.len(),
                    spec.logical_id.0,
                    spec.mnemonic,
                    display_duration(result.duration)
                );
            }
            progress.action_finished(&spec.logical_id.0, false, result.duration);
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
        events.push(BuildEvent {
            action: spec.logical_id.0.clone(),
            digest,
            cache: cache_source,
            duration_ms: t_action.elapsed().as_millis() as u64,
            outcome: "success",
        });
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
    let cache_duration = t_schedule.elapsed().saturating_sub(execute_duration);
    progress.phase_finished(
        "Checking cache",
        cache_duration,
        &format!(
            "{}/{} · {actions_cached} hits · {actions_executed} misses",
            actions_cached + actions_executed,
            actions_cached + actions_executed
        ),
    );
    progress.phase_finished(
        "Executing",
        execute_duration,
        &format!("{actions_executed} actions"),
    );
    progress.phase_started("Materializing");
    progress.phase_finished("Materializing", std::time::Duration::ZERO, "0 artifacts");

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
        if json_output() {
            eprintln!("{result_line} ({id})");
        } else {
            println!("{result_line} ({id})");
        }
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
    if json_output() {
        eprintln!("tests: {passed} passed, {failed_tests} failed");
    } else {
        println!();
        println!("tests: {passed} passed, {failed_tests} failed");
    }

    let t_finish = std::time::Instant::now();
    progress.phase_started("Finishing");
    let state_changed = record_state(
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
    record_events(root, &prepared.store, &events);
    progress.phase_finished(
        "Finishing",
        t_finish.elapsed(),
        if state_changed {
            "state updated"
        } else {
            "state unchanged"
        },
    );
    progress.event(
        "build-finished",
        serde_json::json!({
            "profile": options.profile,
            "actions": recorded.len(),
            "cached": actions_cached,
            "executed": actions_executed,
            "duration_ms": t_total.elapsed().as_millis() as u64,
            "outcome": if failed || failed_tests > 0 { "failure" } else { "success" },
        }),
    );
    if matches!(output_options().message_format, MessageFormat::Human) {
        eprintln!(
            "Finished {} · {actions_cached} cached, {actions_executed} executed {}",
            options.profile,
            display_duration(t_total.elapsed())
        );
    }

    Ok(if failed || failed_tests > 0 { 1 } else { 0 })
}

/// Runs the workspace's benchmark targets (`tong bench`); benchmark runs
/// are test-run actions selected by `options.kinds`.
pub fn bench(
    root: &Path,
    label: Option<&str>,
    libtest_args: &[String],
    options: &BuildOptions,
) -> Result<i32, BuildError> {
    test(root, label, libtest_args, options)
}

/// Whether a test-run id (`rust:test-run:<pkg>:<kind>:<name>`) matches a
/// label: the target name, package name, or `pkg:name`.
fn test_run_matches(logical_id: &str, label: &str) -> bool {
    let rest = logical_id
        .strip_prefix("rust:test-run:")
        .unwrap_or(logical_id);
    let (pkg_and_kind, name) = rest.rsplit_once(':').unwrap_or((rest, ""));
    let pkg = pkg_and_kind
        .rsplit_once(':')
        .map(|(pkg, _kind)| pkg)
        .unwrap_or(pkg_and_kind);
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
    model: tong_rust::RustModel,
    cas: Cas,
    cache: ActionCache,
    executor: LocalExecutor,
    planned: Vec<tong_graph::PlannedAction>,
    artifacts: Vec<tong_rust::FinalArtifact>,
    /// Captured source-tree digests of every package (used by the
    /// deps-only manifest so GC keeps the local packages' trees).
    source_trees: Vec<tong_core::digest::Digest>,
    previous_actions: BTreeMap<String, tong_store::RecordedAction>,
    /// Topological order as indices into `planned`.
    order: Vec<usize>,
}

/// Shared build/test setup: store → model → features → toolchain → plan.
fn prepare(
    root: &Path,
    options: &BuildOptions,
    include_dev_deps: bool,
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
    if root.join("Tong.lock").is_file() {
        let lock = tong_fetch::TongLock::load(root)
            .map_err(|err| BuildError::Manifest(err.to_string()))?;
        let stale = stale_lockfile_packages(root, &lock);
        if !stale.is_empty() && (options.offline || options.locked) {
            return Err(BuildError::Offline(stale_lockfile_message(&stale)));
        }
        if !stale.is_empty() {
            eprintln!("tong: warning: {}", stale_lockfile_message(&stale));
        }
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
    let (mut model, feature_map, configured_members, toolchain, sources) = std::thread::scope(
        |scope| -> Result<
            (
                tong_rust::RustModel,
                tong_rust::FeatureMap,
                Vec<tong_rust::PackageId>,
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
            let configured_members = requests
                .iter()
                .map(|request| request.package.clone())
                .collect();
            let host_triple = tong_rust::host_triple()?;
            let configured_triple = options.target_triple.as_deref().unwrap_or(&host_triple);
            let feature_map = tong_rust::resolve_features_for_target(
                &model,
                &requests,
                include_dev_deps,
                configured_triple,
                &host_triple,
            )
            .map_err(|err| BuildError::Manifest(err.to_string()))?;

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
            Ok((model, feature_map, configured_members, toolchain, sources))
        },
    )?;
    model.feature_map = feature_map;
    model.configured_members = configured_members;
    model.configured_targets = options
        .target_selections
        .iter()
        .map(|selection| (selection.kind.to_owned(), selection.name.clone()))
        .collect();
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
    let previous_actions = project_hash
        .as_ref()
        .and_then(|project_hash| state.latest(project_hash))
        .map(|manifest| {
            manifest
                .actions
                .into_iter()
                .map(|action| (action.logical_id.clone(), action))
                .collect()
        })
        .unwrap_or_default();
    let tests_enabled = options.kinds & (KIND_TEST | KIND_BENCH) != 0;
    let examples_enabled = options.kinds & (KIND_EXAMPLE | KIND_BENCH) != 0;
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
        examples_enabled,
        options.no_run || options.check,
        options.check,
        options.target_triple.clone(),
    )?;
    let planned = backend.plan()?;
    let artifacts = backend.final_artifacts();
    let source_trees = backend.captured_source_trees();

    let order = match topological_order(&planned) {
        Ok(order) => order,
        Err(cycle) => {
            if !cycle.missing.is_empty() {
                return Err(BuildError::Cycle(format!(
                    "planned actions reference unplanned actions: {:?}",
                    cycle.missing
                )));
            }
            return Err(BuildError::Cycle(format!(
                "action cycle or duplicate action ids: {:?}",
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
        model,
        cas,
        cache,
        executor,
        planned,
        artifacts,
        source_trees,
        previous_actions,
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
) -> Result<bool, BuildError> {
    if let Ok(project_hash) = project_hash(root) {
        let state = StateStore::open(&prepared.store)?;
        let previous = state.latest(&project_hash);
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
        if deps_only && let Some(previous) = &previous {
            // Union with the previous closure (dedup by digest): the
            // deps-only build re-verified nothing local, so the previous
            // graph's objects are still reachable from the current
            // sources and must not be swept.
            for digest in &previous.sources {
                if !build_manifest.sources.contains(digest) {
                    build_manifest.sources.push(*digest);
                }
            }
            for digest in &previous.toolchains {
                if !build_manifest.toolchains.contains(digest) {
                    build_manifest.toolchains.push(*digest);
                }
            }
            for action in &previous.actions {
                if !build_manifest
                    .actions
                    .iter()
                    .any(|recorded| recorded.action_digest == action.action_digest)
                {
                    build_manifest.actions.push(action.clone());
                }
            }
            for (name, tree) in &previous.artifacts {
                if !build_manifest.artifacts.iter().any(|(n, _)| n == name) {
                    build_manifest.artifacts.push((name.clone(), *tree));
                }
            }
        }
        if previous
            .as_ref()
            .is_some_and(|previous| same_build_state(previous, &build_manifest))
        {
            return Ok(false);
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
                    Ok(report) if output_options().verbose => eprintln!("{report}"),
                    Ok(_) => {}
                    Err(err) => eprintln!("tong: warning: automatic GC failed: {err}"),
                }
            }
            Err(err) => eprintln!(
                "tong: warning: could not record build state ({}); GC will keep the previous manifest",
                err
            ),
        }
        return Ok(true);
    }
    Ok(false)
}

fn same_build_state(left: &BuildManifest, right: &BuildManifest) -> bool {
    left.schema_version == right.schema_version
        && left.project_hash == right.project_hash
        && left.graph_digest == right.graph_digest
        && left.profiles == right.profiles
        && left.sources == right.sources
        && left.toolchains == right.toolchains
        && left.actions == right.actions
        && left.artifacts == right.artifacts
}

/// One structured build event (one action execution/cache hit).
struct BuildEvent {
    action: String,
    digest: tong_core::digest::Digest,
    cache: &'static str,
    duration_ms: u64,
    outcome: &'static str,
}

/// Appends a build's action events to `state/events/<project_hash>/` as
/// one JSON-lines file per build (bound by the same project retention as
/// the build-state manifests; best-effort).
fn record_events(root: &Path, store: &Path, events: &[BuildEvent]) {
    let Some(project_hash) = project_hash(root).ok() else {
        return;
    };
    let dir = store
        .join("state")
        .join("events")
        .join(project_hash.to_hex());
    if let Err(err) = fs::create_dir_all(&dir) {
        eprintln!("tong: warning: cannot record build events: {err}");
        return;
    }
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = dir.join(format!("{timestamp}-{}.jsonl", std::process::id()));
    let mut lines = Vec::new();
    for event in events {
        lines.push(format!(
            "{{\"action\":\"{}\",\"digest\":\"{}\",\"cache\":\"{}\",\
             \"duration_ms\":{},\"outcome\":\"{}\"}}",
            event.action,
            event.digest.to_hex(),
            event.cache,
            event.duration_ms,
            event.outcome
        ));
    }
    if let Err(err) = fs::write(&path, lines.join("\n")) {
        eprintln!("tong: warning: cannot record build events: {err}");
    }
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
    if !options.excludes.is_empty() && !options.workspace {
        return Err(BuildError::Manifest(
            "--exclude can only be used together with --workspace".to_owned(),
        ));
    }
    let explicit = !options.targets.is_empty() || !options.target_selections.is_empty();
    let roots: Vec<&tong_rust::PackageId> = if explicit {
        model
            .members
            .iter()
            .filter(|id| {
                options.targets.iter().any(|target| {
                    package_spec_matches(target, id)
                        || manifest
                            .is_some_and(|manifest| native_selection_matches(manifest, target, id))
                }) || options.target_selections.iter().any(|selection| {
                    model
                        .packages
                        .iter()
                        .find(|package| &package.id == *id)
                        .is_some_and(|package| target_selection_matches(selection, package))
                })
            })
            .collect()
    } else if options.workspace || model.default_members.is_empty() {
        model.members.iter().collect()
    } else {
        model.default_members.iter().collect()
    };
    let roots: Vec<&tong_rust::PackageId> = roots
        .into_iter()
        .filter(|id| {
            !options
                .excludes
                .iter()
                .any(|exclude| package_spec_matches(exclude, id))
        })
        .collect();
    if roots.is_empty() && (explicit || !model.members.is_empty()) {
        let detail = if explicit {
            format!(
                "no workspace package or target matches packages {:?}, targets {:?}",
                options.targets,
                options
                    .target_selections
                    .iter()
                    .map(|selection| format!("{}:{}", selection.kind, selection.name))
                    .collect::<Vec<_>>()
            )
        } else {
            "workspace selection contains no packages".to_owned()
        };
        return Err(BuildError::Manifest(detail));
    }

    let mut requests = Vec::new();
    if let Some(manifest) = manifest {
        // Native mode: one package per manifest target (cc_import targets
        // are native imports, not feature-bearing packages).
        for id in roots {
            let target = manifest
                .target
                .values()
                .find(|target| target.package_name.as_deref() == Some(&id.name))
                .or_else(|| manifest.target.get(&id.name));
            let mut features = options.features.features.clone();
            if options.features.all_features
                && let Some(target) = target
            {
                features.extend(target.features.keys().cloned());
            }
            let default_features = !options.features.no_default_features;
            requests.push(tong_rust::FeatureRequest {
                package: id.clone(),
                features,
                default_features,
            });
        }
    } else {
        for id in roots {
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
        import_cargo_workspace(
            root,
            &host_triple,
            sources,
            std::env::var("CARGO_ENCODED_RUSTFLAGS").ok().as_deref(),
        )
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
    let mut options = options.clone();
    // A `//member:key` label names the target key; the artifact carries
    // the declared output name. Resolve the label to the output artifact
    // name before building so the assembly filter matches.
    if target.contains('/') || target.starts_with(':') {
        let manifest = load_manifest(root)?;
        let model = load_model_unlocked(root, manifest.as_ref())?;
        if let Some(pkg) = resolve_package_label(root, &manifest, &model, target)
            && let Some(bin) = pkg.bins.first()
        {
            options.targets.clear();
            options.targets.push(bin.name.clone());
        }
    }
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

/// `tong query`: prints the target, dependency, or action graph as text
/// or versioned JSON.
pub fn query(
    root: &Path,
    what: &str,
    label: Option<&str>,
    format: &str,
    options: &BuildOptions,
) -> Result<(), BuildError> {
    let prepared = prepare(root, options, true, &[])?;
    let json = format == "json";
    if json {
        println!("{{\"schema\":1}}");
    }
    match what {
        "targets" => {
            for pkg in &prepared.model.packages {
                let features = prepared
                    .model
                    .feature_map
                    .features_for(&pkg.id, false)
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>();
                let kind = if pkg.lib.is_some() {
                    "lib"
                } else if !pkg.bins.is_empty() {
                    "bin"
                } else if !pkg.tests.is_empty() {
                    "test"
                } else {
                    "other"
                };
                if json {
                    println!(
                        "{{\"label\":\"{}\",\"package\":\"{}\",\"kind\":\"{kind}\",\"features\":{:?}}}",
                        pkg_label_for_query(&pkg.id),
                        pkg.id,
                        features
                    );
                } else {
                    println!("{} kind={kind} features={features:?}", pkg.id);
                }
            }
        }
        "deps" => {
            let Some(pkg) = (match label {
                Some(label) => {
                    resolve_package_label(root, &prepared.manifest, &prepared.model, label)
                }
                None => prepared.model.packages.first(),
            }) else {
                return Err(BuildError::Manifest(format!(
                    "no package matches label {:?}",
                    label.unwrap_or("")
                )));
            };
            for dep in &pkg.deps {
                if json {
                    println!(
                        "{{\"extern\":\"{}\",\"package\":\"{}\",\"optional\":{}}}",
                        dep.extern_name, dep.package, dep.optional
                    );
                } else {
                    println!("{} -> {} (extern {})", pkg.id, dep.package, dep.extern_name);
                }
            }
        }
        "actions" => {
            let Some(pkg) = (match label {
                Some(label) => {
                    resolve_package_label(root, &prepared.manifest, &prepared.model, label)
                }
                None => prepared.model.packages.first(),
            }) else {
                return Err(BuildError::Manifest(format!(
                    "no package matches label {:?}",
                    label.unwrap_or("")
                )));
            };
            for action in &prepared.planned {
                if !action.logical_id.0.contains(&pkg.name)
                    && !action.logical_id.0.contains(&crate_name_dash(&pkg.name))
                {
                    continue;
                }
                // Concretization needs completed dependencies; on a fresh
                // workspace report the planned identity without a digest.
                let spec = (action.make)(&CompletedMap(BTreeMap::new()), &prepared.cas);
                let digest = match &spec {
                    Ok(spec) => spec.digest().to_hex(),
                    Err(_) => "pending".to_owned(),
                };
                let spec = spec.ok();
                if json {
                    println!(
                        "{{\"logical_id\":\"{}\",\"digest\":\"{}\",\"mnemonic\":\"{}\",\"deps\":{:?},\"cacheable\":{},\"platform\":\"{}\"}}",
                        action.logical_id.0,
                        digest,
                        action.mnemonic,
                        action.deps.iter().map(|d| d.0.clone()).collect::<Vec<_>>(),
                        spec.as_ref().is_none_or(
                            |s| s.cache_policy == tong_core::action::CachePolicy::Enabled
                        ),
                        spec.as_ref()
                            .map(|s| format!("{:?}", s.execution_platform))
                            .unwrap_or_default()
                    );
                } else {
                    println!(
                        "{} ({}) digest={}",
                        action.logical_id.0, action.mnemonic, digest
                    );
                }
            }
        }
        other => {
            return Err(BuildError::Manifest(format!(
                "unknown query {other:?}; expected \"targets\", \"deps\", or \"actions\""
            )));
        }
    }
    Ok(())
}

fn crate_name_dash(name: &str) -> String {
    name.replace('_', "-")
}

/// The canonical label of a package (`//<member-path>:<name>`).
fn pkg_label_for_query(id: &tong_rust::PackageId) -> String {
    let rel = match &id.source {
        tong_rust::SourceId::Workspace(rel) => rel.clone(),
        _ => return id.name.clone(),
    };
    if rel == "." {
        format!(":{}", id.name)
    } else {
        format!("//{rel}:{}", id.name)
    }
}

/// `tong graph`: prints the planned action graph as JSON or DOT.
pub fn graph(root: &Path, format: &str, options: &BuildOptions) -> Result<(), BuildError> {
    let prepared = prepare(root, options, true, &[])?;
    match format {
        "json" => {
            graph_json(&prepared, options)?;
        }
        "dot" => {
            println!("digraph tong {{");
            for action in &prepared.planned {
                for dep in &action.deps {
                    println!("  \"{}\" -> \"{}\"", dep.0, action.logical_id.0);
                }
            }
            println!("}}");
        }
        other => {
            return Err(BuildError::Manifest(format!(
                "unknown graph format {other:?}; expected \"json\" or \"dot\""
            )));
        }
    }
    Ok(())
}

/// `tong graph --format json`: the resolver view (package identities,
/// resolved features, active dependency edges, target kinds) followed by
/// the planned action graph. The resolver view is the differential corpus
/// surface compared against `cargo metadata`; the node list mirrors the
/// old action graph.
fn graph_json(prepared: &Prepared, options: &BuildOptions) -> Result<(), BuildError> {
    let model = &prepared.model;
    let requests = feature_requests(model, options, prepared.manifest.as_ref())?;
    let all_platform_map = tong_rust::resolve_all_platform_features(model, &requests, true)
        .map_err(|error| BuildError::Manifest(error.to_string()))?;
    let map = &all_platform_map;
    // Tong.lock contains Cargo's all-target, all-optional package set;
    // Cargo metadata's resolve view contains only packages reachable from
    // configured roots through active edges. Keep those two graphs
    // separate at this presentation boundary.
    let mut reachable: BTreeSet<tong_rust::PackageId> = if model.configured_members.is_empty() {
        model.members.iter().cloned().collect()
    } else {
        model.configured_members.iter().cloned().collect()
    };
    loop {
        let before = reachable.len();
        for pkg in &model.packages {
            if !reachable.contains(&pkg.id) {
                continue;
            }
            for dep in pkg
                .deps
                .iter()
                .chain(pkg.build_deps.iter())
                .chain(pkg.dev_deps.iter())
            {
                if !dep.optional || map.edge_active(&pkg.id, &dep.extern_name) {
                    reachable.insert(dep.package.clone());
                }
            }
        }
        if reachable.len() == before {
            break;
        }
    }
    let packages: Vec<&tong_rust::Package> = model
        .packages
        .iter()
        .filter(|package| reachable.contains(&package.id))
        .collect();

    println!("{{\"schema\":1,\"packages\":[");
    for (index, pkg) in packages.iter().enumerate() {
        let comma = if index + 1 < packages.len() { "," } else { "" };
        // Cargo metadata reports the union of features activated by every
        // configured unit even though resolver 2/3 keep host and target
        // feature domains separate for compilation.
        let mut feature_set: BTreeSet<&str> = map
            .features_for(&pkg.id, false)
            .iter()
            .map(String::as_str)
            .collect();
        feature_set.extend(map.features_for(&pkg.id, true).iter().map(String::as_str));
        let features: Vec<&str> = feature_set.into_iter().collect();
        // Active edges: non-optional deps plus activated optional edges,
        // across normal/dev/build kinds.
        let mut edges: Vec<String> = Vec::new();
        for dep in pkg
            .deps
            .iter()
            .chain(pkg.build_deps.iter())
            .chain(pkg.dev_deps.iter())
        {
            // Cargo's resolve-node edges cover activated dependencies
            // (weak-ref-activated optionals included); inactive optional
            // edges stay out of the differential view.
            if dep.optional && !map.edge_active(&pkg.id, &dep.extern_name) {
                continue;
            }
            let kind = if pkg
                .dev_deps
                .iter()
                .any(|d| d.extern_name == dep.extern_name)
            {
                "dev"
            } else if pkg
                .build_deps
                .iter()
                .any(|d| d.extern_name == dep.extern_name)
            {
                "build"
            } else {
                "normal"
            };
            let package_label = format!("{}@{}", dep.package.name, dep.package.version);
            edges.push(format!(
                "{{\"package\":\"{package_label}\",\"source\":\"{}\",\"kind\":\"{kind}\",\"optional\":{},\"default_features\":{},\"features\":{:?},\"target\":{}}}",
                dep.package.lock_source(),
                dep.optional,
                dep.default_features,
                dep.features,
                dep.target
                    .as_ref()
                    .map(|t| {
                        // cfg expressions contain quotes; JSON-escape them.
                        let escaped = t.replace('\\', "\\\\").replace('"', "\\\"");
                        format!("\"{escaped}\"")
                    })
                    .unwrap_or_else(|| "null".to_owned())
            ));
        }
        // Target kinds, mirroring cargo's `targets[].kind`.
        let mut targets: Vec<String> = Vec::new();
        if let Some(lib) = &pkg.lib {
            let kind = if lib.proc_macro {
                "proc-macro"
            } else {
                lib.crate_types
                    .first()
                    .map(|crate_type| crate_type.to_rustc())
                    .unwrap_or("lib")
            };
            // Cargo's default lib target name is the SANITIZED package
            // name (sharded-slab's lib target is `sharded_slab`).
            let lib_name = lib
                .name
                .clone()
                .unwrap_or_else(|| tong_rust::model::crate_name(&pkg.name));
            targets.push(format!("{{\"kind\":\"{kind}\",\"name\":\"{lib_name}\"}}"));
        }
        if let Some(script) = &pkg.build_script {
            let stem = script
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or("build");
            targets.push(format!(
                "{{\"kind\":\"custom-build\",\"name\":\"build-script-{stem}\"}}"
            ));
        }
        for bin in &pkg.bins {
            targets.push(format!("{{\"kind\":\"bin\",\"name\":\"{}\"}}", bin.name));
        }
        for example in &pkg.examples {
            targets.push(format!(
                "{{\"kind\":\"example\",\"name\":\"{}\"}}",
                example.name
            ));
        }
        for test in &pkg.tests {
            // The auto-derived lib unit test is not a declared cargo
            // target (`cargo metadata` omits it); skip it here so target
            // kinds compare 1:1.
            if pkg.lib.as_ref().is_some_and(|lib| lib.path == test.path) {
                continue;
            }
            let kind = if test.doc {
                "doc-test"
            } else if test.bench {
                "bench"
            } else {
                "test"
            };
            targets.push(format!(
                "{{\"kind\":\"{kind}\",\"name\":\"{}\"}}",
                test.name
            ));
        }
        println!(
            "{{\"id\":\"{}\",\"name\":\"{}\",\"version\":\"{}\",\"source\":\"{}\",\"features\":{:?},\"edges\":[{}],\"targets\":[{}]}}{comma}",
            pkg.id,
            pkg.name,
            pkg.version,
            pkg.id.lock_source(),
            features,
            edges.join(","),
            targets.join(",")
        );
    }
    println!("],\"nodes\":[");
    for (index, action) in prepared.planned.iter().enumerate() {
        let comma = if index + 1 < prepared.planned.len() {
            ","
        } else {
            ""
        };
        println!(
            "{{\"id\":\"{}\",\"mnemonic\":\"{}\",\"deps\":{:?}}}{comma}",
            action.logical_id.0,
            action.mnemonic,
            action.deps.iter().map(|d| d.0.clone()).collect::<Vec<_>>()
        );
    }
    println!("]}}");
    Ok(())
}

/// `tong explain rebuild <label>`: compares the newest two build records
/// and reports the first changed semantic field of the label's actions.
pub fn explain(
    root: &Path,
    what: &str,
    label: &str,
    _options: &BuildOptions,
) -> Result<(), BuildError> {
    let _ = _options;
    if what != "rebuild" {
        return Err(BuildError::Manifest(format!(
            "unknown explanation {what:?}; expected \"rebuild\""
        )));
    }
    let manifest = load_manifest(root)?;
    let store = store_dir(root, manifest.as_ref())?;
    let state = StateStore::open(&store)?;
    let project_hash = project_hash(root)
        .map_err(|_| BuildError::Manifest("cannot compute the project hash".to_owned()))?;
    let latest = state.latest(&project_hash).ok_or_else(|| {
        BuildError::Manifest("no build records yet; run a build first".to_owned())
    })?;
    let previous = state
        .history(&project_hash)
        .into_iter()
        .nth(1)
        .ok_or_else(|| {
            BuildError::Manifest("only one build record; run another build first".to_owned())
        })?;
    let mut matched = false;
    for action in &latest.actions {
        if !action.logical_id.contains(label) {
            continue;
        }
        matched = true;
        let old = previous
            .actions
            .iter()
            .find(|a| a.logical_id == action.logical_id);
        let Some(old) = old else {
            println!("{}: new action", action.logical_id);
            continue;
        };
        if old.action_digest == action.action_digest {
            continue;
        }
        let field = if old.input_root != action.input_root {
            format!("input (tree {})", action.input_root.digest())
        } else if old.env_bundle != action.env_bundle {
            "toolchain".to_owned()
        } else if old.executable != action.executable {
            "executable".to_owned()
        } else {
            "args/env/policy".to_owned()
        };
        println!("{}: changed {field}", action.logical_id);
    }
    if !matched {
        return Err(BuildError::Manifest(format!(
            "no recorded action matches label {label:?}"
        )));
    }
    Ok(())
}

/// `tong log`: prints structured build events from `state/events/`.
pub fn log(root: &Path, format: &str, _options: &BuildOptions) -> Result<(), BuildError> {
    let _ = _options;
    let manifest = load_manifest(root)?;
    let store = store_dir(root, manifest.as_ref())?;
    let Some(project_hash) = project_hash(root).ok() else {
        return Ok(());
    };
    let events_dir = store
        .join("state")
        .join("events")
        .join(project_hash.to_hex());
    let entries = match fs::read_dir(&events_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            println!("no build events recorded");
            return Ok(());
        }
        Err(err) => return Err(BuildError::Io(err)),
    };
    for entry in entries {
        let entry = entry.map_err(BuildError::Io)?;
        let text = fs::read_to_string(entry.path()).map_err(BuildError::Io)?;
        for line in text.lines().filter(|line| !line.is_empty()) {
            if format == "json" {
                println!("{line}");
            } else {
                let event: serde_json::Value = serde_json::from_str(line).unwrap_or_default();
                let action = event["action"].as_str().unwrap_or("");
                let source = event["cache"].as_str().unwrap_or("");
                let duration = event["duration_ms"].as_u64().unwrap_or(0);
                println!("{action} cache={source} duration_ms={duration}");
            }
        }
    }
    Ok(())
}

/// Resolves a target label (`:name`, `//member:name`, or a plain name) to
/// the workspace package it names, mapping native target keys through the
/// root/member manifests to their declared `package_name`.
fn resolve_package_label<'a>(
    root: &Path,
    manifest: &Option<Manifest>,
    model: &'a tong_rust::RustModel,
    label: &str,
) -> Option<&'a tong_rust::Package> {
    let name = label
        .strip_prefix(':')
        .or_else(|| label.rsplit_once(':').map(|(_, name)| name))
        .unwrap_or(label);
    let mut matched: Vec<&tong_rust::Package> = model
        .packages
        .iter()
        .filter(|p| p.name == name || artifact_name_matches(name, &p.name))
        .collect();
    if matched.is_empty() {
        // Native labels name the target KEY (`[target.app]` in the root
        // or the member manifest), which maps to the declared
        // `package_name`.
        let member_dir = label
            .strip_prefix("//")
            .and_then(|rest| rest.rsplit_once(':').map(|(dir, _)| dir))
            .filter(|dir| !dir.is_empty());
        let member_manifest =
            member_dir.and_then(|dir| tong_graph::manifest::Manifest::load(&root.join(dir)).ok());
        let package_name = member_manifest
            .as_ref()
            .or(manifest.as_ref())
            .and_then(|manifest| manifest.target.get(name))
            .and_then(|target| target.package_name.clone())
            .unwrap_or_else(|| name.to_owned());
        matched = model
            .packages
            .iter()
            .filter(|p| p.name == package_name)
            .collect();
    }
    matched.first().copied()
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

fn package_spec_matches(spec: &str, package: &tong_rust::PackageId) -> bool {
    if artifact_name_matches(spec, &package.name) {
        return true;
    }
    let (identity, source) = spec
        .split_once('#')
        .map_or((spec, None), |(identity, source)| (identity, Some(source)));
    let Some((name, version)) = identity.rsplit_once('@') else {
        return false;
    };
    artifact_name_matches(name, &package.name)
        && version == package.version.to_string()
        && source.is_none_or(|source| source == package.lock_source())
}

fn target_selection_matches(selection: &TargetSelection, package: &tong_rust::Package) -> bool {
    match selection.kind {
        "bin" => package
            .bins
            .iter()
            .any(|target| target.name == selection.name),
        "example" => package
            .examples
            .iter()
            .any(|target| target.name == selection.name),
        "test" => package
            .tests
            .iter()
            .any(|target| !target.bench && target.name == selection.name),
        "bench" => package
            .tests
            .iter()
            .any(|target| target.bench && target.name == selection.name),
        _ => false,
    }
}

fn native_selection_matches(
    manifest: &Manifest,
    selection: &str,
    package: &tong_rust::PackageId,
) -> bool {
    if let Some((member, _)) = selection
        .strip_prefix("//")
        .and_then(|rest| rest.rsplit_once(':'))
        && let tong_rust::SourceId::Workspace(relative) = &package.source
        && member == relative
    {
        return true;
    }
    let target_name = selection
        .strip_prefix(':')
        .or_else(|| {
            selection
                .strip_prefix("//")
                .and_then(|rest| rest.rsplit_once(':').map(|(_, name)| name))
        })
        .unwrap_or(selection);
    manifest
        .target
        .get(target_name)
        .is_some_and(|target| target.package_name.as_deref().unwrap_or(target_name) == package.name)
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

/// Reuses a previous result across the one-time transition from a
/// conservative input root to a dep-info-narrowed root. Replacing the new
/// root with the recorded old root must reproduce the exact old action
/// digest, proving every other semantic field is unchanged; the retained
/// tree must also be an exact projection of the old tree.
fn reuse_narrowed_result(
    prepared: &Prepared,
    spec: &tong_core::action::ActionSpec,
    digest: tong_core::digest::Digest,
    verifier: &mut ClosureVerifier,
) -> Result<Option<CachedResult>, BuildError> {
    let Some(previous) = prepared.previous_actions.get(&spec.logical_id.0) else {
        return Ok(None);
    };
    let mut old_root_spec = spec.clone();
    old_root_spec.input_root = previous.input_root;
    if old_root_spec.digest() != previous.action_digest
        || !prepared
            .cas
            .tree_contains(previous.input_root, spec.input_root)?
    {
        return Ok(None);
    }
    let result = CachedResult {
        outputs: previous.outputs,
        stdout: previous.stdout,
        stderr: previous.stderr,
        duration_millis: previous.duration_millis,
    };
    if !result.is_complete_cached(&prepared.cas, verifier)? {
        return Ok(None);
    }
    prepared.cache.put(digest, &result)?;
    Ok(Some(result))
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
    /// Workspace root (resolves `path+<rel>` locked sources to member
    /// directories — `[patch]`-replaced crates like serde's facade).
    root: PathBuf,
    lock: Option<tong_fetch::TongLock>,
    store: PathBuf,
    cas: Cas,
    /// Transient checkouts materialized during this build (removed by the
    /// driver after the source trees are captured).
    materialized: std::cell::RefCell<Vec<PathBuf>>,
}

fn locked_source_matches_edge(edge: &tong_rust::RegistryEdge, source: &str) -> bool {
    match &edge.git {
        Some(selector) => source
            .strip_prefix("git+")
            .and_then(|source| source.split_once('#'))
            .is_some_and(|(url, _)| url == selector.url),
        None => source.starts_with("registry+"),
    }
}

impl LockfileSource {
    fn new(root: &Path, store: &Path, cas: Cas) -> Self {
        let lock = tong_fetch::TongLock::load(root).ok();
        Self {
            root: root.to_path_buf(),
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
        {
            for dep in &parent_entry.dependencies {
                let (name, version, source) = tong_fetch::LockedPackage::parse_dependency(dep);
                if name != edge.package || !locked_source_matches_edge(edge, source) {
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
                // Renamed dependencies can pin several versions of one
                // package (combine's `bytes_05 = { package = "bytes",
                // version = "0.5" }` beside `bytes = "1"`): match the
                // requirement, not just the name.
                if req.matches(&package.version) {
                    return Ok(Some(package.clone()));
                }
            }
            // The parent's locked edges do not include a version matching
            // this requirement: an inactive optional edge, or a stale
            // lock.
            if edge.optional {
                return Ok(None);
            }
            return Err(BuildError::Offline(format!(
                "lockfile out of date: `{}` requires {} but the locked graph of `{}` \
                 does not include it; run `tong lock`",
                edge.parent.name, edge.req, edge.parent.name
            )));
        }

        // Version-1 fallback: requirement matching over the lock, which the
        // v1 migration kept unambiguous.
        let candidates: Vec<&tong_fetch::LockedPackage> = lock
            .candidates(&edge.package)
            .filter(|package| {
                req.matches(&package.version) && locked_source_matches_edge(edge, &package.source)
            })
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
                source_tree: Some(tong_core::artifact::TreeDigest::new(tree_digest)),
                propagate_source: true,
            }));
        }
        // `[patch]`-replaced crates lock to a workspace/path member: the
        // edge's source is `path+<rel>` — return the member directory
        // instead of a registry checkout.
        if let Some(rel) = package.source.strip_prefix("path+") {
            let id = tong_rust::model::PackageId {
                name: package.name.clone(),
                version: package.version.clone(),
                source: tong_rust::model::SourceId::parse_lock_source(&package.source)
                    .map_err(tong_rust::CargoImportError::Unsupported)?,
            };
            return Ok(Some(tong_rust::LockedSource {
                id,
                source_dir: self.root.join(rel),
                source_tree: None,
                propagate_source: false,
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
        let source_tree = if let Some(tree) = tong_fetch::source_tree_digest(&self.store, checksum)
        {
            tree
        } else {
            let tree = self.cas.capture_dir(&source_dir).map_err(|err| {
                tong_rust::CargoImportError::Unsupported(format!(
                    "cannot capture locked source {} {}: {err}",
                    package.name, package.version
                ))
            })?;
            tong_fetch::record_source_tree(&self.store, checksum, tree)
                .map_err(|err| tong_rust::CargoImportError::Unsupported(err.to_string()))?;
            tree
        };
        let source = tong_rust::model::SourceId::parse_lock_source(&package.source)
            .map_err(tong_rust::CargoImportError::Unsupported)?;
        Ok(Some(tong_rust::LockedSource {
            id: tong_rust::model::PackageId {
                name: package.name,
                version: package.version,
                source,
            },
            source_dir,
            source_tree: Some(source_tree),
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

    /// A full Cargo.lock commit can expand a short manifest `rev` before
    /// Tong has captured and fingerprinted the source tree.
    fn locked_git_commit(&self, name: &str, url: &str) -> Option<String> {
        let prefix = format!("git+{url}#");
        self.preferences
            .as_ref()?
            .candidates(name)
            .find_map(|package| package.source.strip_prefix(&prefix).map(str::to_owned))
    }
}

impl tong_rust::LockedSourceProvider for CollectProvider {
    fn collecting(&self) -> bool {
        true
    }

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
        let locked_commit = self.locked_git_commit(&edge.package, &selector.url);
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
            locked_commit.as_deref().or(selector.rev.as_deref()),
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
            source_tree: Some(resolved.tree_digest),
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

/// Seeds resolver preferences and exact dependency edges from Cargo.lock.
/// Registry sources are remapped to Tong's configured index; existing
/// Tong path/git entries supply their source and content fingerprints.
fn seed_from_cargo_lock(
    root: &Path,
    registry_source: &str,
    existing: &tong_fetch::TongLock,
) -> Result<tong_fetch::TongLock, BuildError> {
    #[derive(serde::Deserialize)]
    struct CargoLock {
        #[serde(default)]
        package: Vec<CargoLockPackage>,
    }
    #[derive(Clone, serde::Deserialize)]
    struct CargoLockPackage {
        name: String,
        version: semver::Version,
        source: Option<String>,
        checksum: Option<String>,
        #[serde(default)]
        dependencies: Vec<String>,
    }
    let text = fs::read_to_string(root.join("Cargo.lock"))
        .map_err(|err| BuildError::Manifest(format!("cannot read Cargo.lock: {err}")))?;
    let cargo: CargoLock = toml::from_str(&text)
        .map_err(|err| BuildError::Manifest(format!("cannot parse Cargo.lock: {err}")))?;
    let mut lock = tong_fetch::TongLock {
        version: tong_fetch::LOCKFILE_VERSION,
        packages: Vec::new(),
    };
    let cargo_packages = cargo.package;
    let mapped_source = |package: &CargoLockPackage| {
        if package
            .source
            .as_deref()
            .is_some_and(|source| source.starts_with("registry+"))
        {
            Some(registry_source.to_owned())
        } else if let Some(source) = package
            .source
            .as_deref()
            .and_then(|source| source.strip_prefix("git+"))
            && let Some((location, commit)) = source.rsplit_once('#')
        {
            let url = location
                .split('?')
                .next()
                .unwrap_or(location)
                .trim_end_matches(".git");
            Some(format!("git+{url}#{commit}"))
        } else {
            existing
                .candidates(&package.name)
                .find(|candidate| {
                    candidate.version == package.version
                        && match package.source.as_deref() {
                            Some(source) if source.starts_with("git+") => {
                                candidate.source.starts_with("git+")
                            }
                            None => candidate.source.starts_with("path+"),
                            _ => false,
                        }
                })
                .map(|candidate| candidate.source.clone())
        }
    };
    for package in &cargo_packages {
        if let Some(source) = mapped_source(package) {
            let mut dependencies = Vec::new();
            for dependency in &package.dependencies {
                let (head, explicit_source) = dependency
                    .strip_suffix(')')
                    .and_then(|text| text.rsplit_once(" ("))
                    .map(|(head, source)| (head, Some(source)))
                    .unwrap_or((dependency.as_str(), None));
                let (name, explicit_version) = head
                    .rsplit_once(' ')
                    .filter(|(_, version)| semver::Version::parse(version).is_ok())
                    .map(|(name, version)| (name, Some(version)))
                    .unwrap_or((head, None));
                let candidate = cargo_packages.iter().find(|candidate| {
                    candidate.name == name
                        && explicit_version
                            .is_none_or(|version| candidate.version.to_string() == version)
                        && explicit_source
                            .is_none_or(|source| candidate.source.as_deref() == Some(source))
                });
                if let Some(candidate) = candidate
                    && let Some(source) = mapped_source(candidate)
                {
                    dependencies.push(format!("{} {} {source}", candidate.name, candidate.version));
                }
            }
            let prior = existing.exact(&package.name, &package.version, &source);
            let registry = source.starts_with("registry+");
            lock.packages.push(tong_fetch::LockedPackage {
                name: package.name.clone(),
                version: package.version.clone(),
                source,
                checksum: registry.then(|| package.checksum.clone()).flatten(),
                yanked: prior.is_some_and(|package| package.yanked),
                publish_time: prior.and_then(|package| package.publish_time.clone()),
                manifest_checksum: prior.and_then(|package| package.manifest_checksum.clone()),
                tree_digest: prior.and_then(|package| package.tree_digest.clone()),
                dependencies,
            });
        }
    }
    for package in &existing.packages {
        if !package.source.starts_with("registry+")
            && lock
                .exact(&package.name, &package.version, &package.source)
                .is_none()
        {
            lock.packages.push(package.clone());
        }
    }
    Ok(lock)
}

fn lock_with(root: &Path, offline: bool, drop_preference: Option<&str>) -> Result<(), BuildError> {
    let t_total = std::time::Instant::now();
    let progress = Progress::new();
    let t_analyze = std::time::Instant::now();
    progress.phase_started("Analyzing manifests");
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
    let existing_preferences = tong_fetch::TongLock::load(root).unwrap_or_default();
    let mut preferences = existing_preferences.clone();
    // In Cargo-import mode, an ordinary `tong lock` follows Cargo.lock's
    // exact registry selection. This makes adopting Tong deterministic and
    // avoids silently upgrading dependencies merely because Tong.lock was
    // generated by an older Tong. Keep Tong's git/path pins, since Cargo's
    // lock format does not contain the source trees Tong needs offline.
    // `tong update` deliberately keeps Tong.lock as its preference source.
    if drop_preference.is_none() && root.join("Cargo.lock").is_file() {
        let registry_source = format!("registry+{}", registry.index_url);
        preferences = seed_from_cargo_lock(root, &registry_source, &existing_preferences)?;
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
    // edges are filtered by the resolved feature map; Cargo.lock-seeded
    // dependency tuples are merged back below for all-target completeness.
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
                name: dep.extern_name.clone(),
                package: Some(dep.package.name.clone()),
                req: None,
                optional: false,
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
            let mut features = edge.features.clone();
            if let Some(resolved) = feature_map.unresolved_features_for(&pkg.id, &edge.extern_name)
            {
                for feature in resolved {
                    if !features.contains(feature) {
                        features.push(feature.clone());
                    }
                }
            }
            deps.push(tong_fetch::ResolvedDep {
                name: edge.extern_name.clone(),
                package: Some(edge.package.clone()),
                req: Some(req),
                optional: false,
                dev,
                features,
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
    if !json_output() {
        println!(
            "resolving {} packages ({} registry edges) against {}",
            locals.len(),
            registry_edges,
            registry.index_url
        );
    }
    let analyze_duration = t_analyze.elapsed();
    progress.phase_finished(
        "Analyzing manifests",
        analyze_duration,
        &format!("{} local packages", locals.len()),
    );
    tracing::debug!(
        target: "tong::perf",
        phase = "lock.analyze",
        packages = locals.len(),
        duration_ms = analyze_duration.as_millis() as u64,
    );
    let t_resolve = std::time::Instant::now();
    progress.phase_started("Resolving lockfile");
    // `[patch]` applies to Cargo workspaces only (`manifest` is `None`
    // for Cargo mode); native Tong.toml roots have no Cargo.toml.
    let patched: std::collections::BTreeSet<String> = if manifest.is_none() {
        tong_rust::cargo_patch_names(root)
            .map_err(|err| BuildError::Manifest(err.to_string()))?
            .into_iter()
            .collect()
    } else {
        std::collections::BTreeSet::new()
    };
    let resolved = tong_fetch::resolve(&index, &locals, &preferences, &patched)
        .map_err(|err| BuildError::Manifest(err.to_string()))?;
    tracing::info!(
        target: "tong::lock",
        phase = "lock.resolve",
        packages = resolved.len(),
        duration_ms = t_resolve.elapsed().as_millis() as u64,
    );
    let resolve_duration = t_resolve.elapsed();
    progress.phase_finished(
        "Resolving lockfile",
        resolve_duration,
        &format!("{} packages", resolved.len()),
    );
    tracing::debug!(
        target: "tong::perf",
        phase = "lock.resolve",
        packages = resolved.len(),
        duration_ms = resolve_duration.as_millis() as u64,
    );

    // Assemble the lock: every resolved package, registry or local, with
    // exact per-edge identities (a name may resolve to several versions or
    // sources). Sources come from the resolver — a local `foo` and a
    // registry `foo` never alias.
    let t_write = std::time::Instant::now();
    progress.phase_started("Writing lockfile");
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
        // Cargo-import locking starts from Cargo.lock's all-target graph.
        // Preserve its exact inactive optional edges while the resolver's
        // configured feature closure supplies the active build graph.
        if let Some(preferred) = preferences.exact(&package.name, &package.version, &source) {
            deps.extend(preferred.dependencies.iter().cloned());
        }
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
    // Packages that are lock-only (for example inactive optional Cargo
    // dependencies) do not enter the configured resolver activation set,
    // but their exact identities and edges must remain in Tong.lock.
    for package in &preferences.packages {
        if locked
            .exact(&package.name, &package.version, &package.source)
            .is_none()
        {
            locked.packages.push(package.clone());
        }
    }
    locked
        .save(root)
        .map_err(|err| BuildError::Manifest(err.to_string()))?;
    // Git checkouts are transient at lock time: their content lives in
    // the CAS (the tree digests just recorded); drop the working trees.
    for checkout in git_checkouts {
        let _ = fs::remove_dir_all(checkout);
    }
    let write_duration = t_write.elapsed();
    progress.phase_finished(
        "Writing lockfile",
        write_duration,
        &format!("{} packages", locked.packages.len()),
    );
    tracing::debug!(
        target: "tong::perf",
        phase = "lock.write",
        packages = locked.packages.len(),
        duration_ms = write_duration.as_millis() as u64,
    );
    if !json_output() {
        println!("wrote Tong.lock ({} packages)", locked.packages.len());
    }
    tracing::debug!(
        target: "tong::perf",
        phase = "lock.total",
        duration_ms = t_total.elapsed().as_millis() as u64,
    );
    progress.command_finished(
        if drop_preference.is_some() {
            "update"
        } else {
            "lock"
        },
        t_total.elapsed(),
        serde_json::json!({ "packages": locked.packages.len() }),
    );
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

enum SourceFetchKind {
    Registry(tong_fetch::ResolvedPackage),
    Git {
        url: String,
        commit: String,
        tree: TreeDigest,
    },
}

struct SourceFetchTask {
    name: String,
    version: semver::Version,
    kind: SourceFetchKind,
}

/// Avoid overwhelming registries with one connection per logical CPU on
/// large machines while still keeping typical developer links saturated.
const MAX_FETCH_WORKERS: usize = 8;

/// Fetches independent locked sources concurrently. Results are reported by
/// the caller thread as workers finish so output remains line-oriented, while
/// errors are returned in lockfile order for deterministic diagnostics.
fn fetch_sources_parallel(
    cas: &Cas,
    store: &Path,
    registry: &tong_fetch::RegistryConfig,
    tasks: &[SourceFetchTask],
    progress: &Progress,
) -> Result<(), BuildError> {
    if tasks.is_empty() {
        return Ok(());
    }
    let workers = std::thread::available_parallelism()
        .map(|parallelism| parallelism.get())
        .unwrap_or(4)
        .min(MAX_FETCH_WORKERS)
        .min(tasks.len());
    let next = AtomicUsize::new(0);
    let (sender, receiver) = std::sync::mpsc::channel();
    let mut results = std::thread::scope(|scope| {
        for _ in 0..workers {
            let sender = sender.clone();
            let next = &next;
            scope.spawn(move || {
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(task) = tasks.get(index) else {
                        break;
                    };
                    let started = std::time::Instant::now();
                    let result = match &task.kind {
                        SourceFetchKind::Registry(package) => {
                            let mut registry = registry.clone();
                            tong_fetch::fetch_crate(cas, &mut registry, package)
                                .map(|_| ())
                                .map_err(|err| err.to_string())
                        }
                        SourceFetchKind::Git { url, commit, tree } => {
                            tong_fetch::materialize_tree(cas, *tree, store, url, commit)
                                .map(|_| ())
                                .map_err(|err| format!("{err}; run `tong lock`"))
                        }
                    };
                    let _ = sender.send((index, started.elapsed(), result));
                }
            });
        }
        drop(sender);

        let mut completed = 0usize;
        let mut results = Vec::with_capacity(tasks.len());
        while let Ok((index, duration, result)) = receiver.recv() {
            completed += 1;
            let task = &tasks[index];
            let outcome = if result.is_ok() { "success" } else { "failure" };
            progress.event(
                "fetch-finished",
                serde_json::json!({
                    "package": task.name,
                    "version": task.version,
                    "duration_ms": duration.as_millis() as u64,
                    "outcome": outcome,
                }),
            );
            if matches!(output_options().message_format, MessageFormat::Human) {
                eprintln!(
                    "  {} [{completed}/{}] {} {} {}",
                    if result.is_ok() { "Fetched" } else { "Failed" },
                    tasks.len(),
                    task.name,
                    task.version,
                    display_duration(duration)
                );
            }
            results.push((index, result));
        }
        results
    });
    results.sort_by_key(|(index, _)| *index);
    for (_, result) in results {
        result.map_err(BuildError::Manifest)?;
    }
    Ok(())
}

/// Downloads every locked registry package into the source store; a no-op
/// when everything is already stored. `--offline` never touches the
/// network: missing blobs fail with a targeted diagnostic before any
/// download is attempted.
pub fn fetch(root: &Path, offline: bool) -> Result<(), BuildError> {
    let t_total = std::time::Instant::now();
    let progress = Progress::new();
    let t_inspect = std::time::Instant::now();
    progress.phase_started("Inspecting lockfile");
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
    let stale = stale_lockfile_packages(root, &lock);
    report_stale_lockfile(&progress, &stale);
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
    let mut tasks = Vec::new();
    let mut registry_sources = BTreeSet::new();
    let mut git_sources = BTreeSet::new();
    for package in &lock.packages {
        if package.source.starts_with("registry+") {
            let Some(checksum) = &package.checksum else {
                continue;
            };
            if !registry_sources.insert((
                package.name.clone(),
                package.version.clone(),
                checksum.clone(),
            )) {
                continue;
            }
            let resolved = tong_fetch::ResolvedPackage {
                name: package.name.clone(),
                version: package.version.clone(),
                source: None,
                checksum: Some(checksum.clone()),
                yanked: package.yanked,
                local: false,
                dependencies: Vec::new(),
            };
            tasks.push(SourceFetchTask {
                name: package.name.clone(),
                version: package.version.clone(),
                kind: SourceFetchKind::Registry(resolved),
            });
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
            if !git_sources.insert((url.to_owned(), commit.to_owned(), tree_digest)) {
                continue;
            }
            tasks.push(SourceFetchTask {
                name: package.name.clone(),
                version: package.version.clone(),
                kind: SourceFetchKind::Git {
                    url: url.to_owned(),
                    commit: commit.to_owned(),
                    tree: TreeDigest::new(tree_digest),
                },
            });
        }
    }
    if !offline
        && tasks.iter().any(|task| {
            let SourceFetchKind::Registry(package) = &task.kind else {
                return false;
            };
            package
                .checksum
                .as_deref()
                .is_some_and(|checksum| !tong_fetch::crate_blob_path(&store, checksum).is_file())
        })
    {
        registry
            .ensure_configured()
            .map_err(|err| BuildError::Manifest(err.to_string()))?;
    }
    let inspect_duration = t_inspect.elapsed();
    progress.phase_finished(
        "Inspecting lockfile",
        inspect_duration,
        &format!("{} sources", tasks.len()),
    );
    tracing::debug!(
        target: "tong::perf",
        phase = "fetch.inspect",
        sources = tasks.len(),
        stale = stale.len(),
        duration_ms = inspect_duration.as_millis() as u64,
    );

    let t_fetch = std::time::Instant::now();
    progress.phase_started("Fetching sources");
    fetch_sources_parallel(&cas, &store, &registry, &tasks, &progress)?;
    let fetch_duration = t_fetch.elapsed();
    progress.phase_finished(
        "Fetching sources",
        fetch_duration,
        &format!("{} sources", tasks.len()),
    );
    tracing::debug!(
        target: "tong::perf",
        phase = "fetch.sources",
        sources = tasks.len(),
        duration_ms = fetch_duration.as_millis() as u64,
    );
    tracing::debug!(
        target: "tong::perf",
        phase = "fetch.total",
        duration_ms = t_total.elapsed().as_millis() as u64,
    );
    if !json_output() {
        println!("fetched {} sources", tasks.len());
    }
    progress.command_finished(
        "fetch",
        t_total.elapsed(),
        serde_json::json!({
            "sources": tasks.len(),
            "stale_lockfile": !stale.is_empty(),
        }),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{artifact_name_matches, locked_source_matches_edge};
    use tong_rust::{GitSelector, PackageId, RegistryEdge, SourceId};

    #[test]
    fn artifact_labels_match_exactly_or_with_normalized_separators() {
        assert!(artifact_name_matches(":voxel_city", "voxel_city"));
        assert!(artifact_name_matches(":voxel_city", "voxel-city"));
        assert!(artifact_name_matches(":voxel-city", "voxel_city"));
        assert!(artifact_name_matches("//crates/app:calc-cli", "calc-cli"));
        assert!(artifact_name_matches("calc-cli", "calc-cli"));
        assert!(!artifact_name_matches(":other", "voxel-city"));
    }

    #[test]
    fn locked_sources_match_dependency_source_kind() {
        let mut edge = RegistryEdge {
            parent: PackageId {
                name: "root".to_owned(),
                version: semver::Version::new(1, 0, 0),
                source: SourceId::Workspace(".".to_owned()),
            },
            extern_name: "shared".to_owned(),
            package: "shared".to_owned(),
            req: "*".to_owned(),
            git: None,
            optional: false,
            default_features: true,
            features: Vec::new(),
        };
        assert!(locked_source_matches_edge(
            &edge,
            "registry+https://github.com/rust-lang/crates.io-index"
        ));
        assert!(!locked_source_matches_edge(
            &edge,
            "git+https://example.com/shared#0123456789abcdef"
        ));

        edge.git = Some(GitSelector {
            url: "https://example.com/shared".to_owned(),
            rev: None,
            tag: None,
            branch: None,
        });
        assert!(locked_source_matches_edge(
            &edge,
            "git+https://example.com/shared#0123456789abcdef"
        ));
        assert!(!locked_source_matches_edge(
            &edge,
            "git+https://example.com/other#0123456789abcdef"
        ));
    }
}
