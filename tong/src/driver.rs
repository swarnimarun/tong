//! The build driver: manifests → toolchain → plan → schedule → execute →
//! assemble (PLAN.md section 15, Phase 1 pipeline).

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use tong_core::action::ActionId;
use tong_core::artifact::TreeDigest;
use tong_exec::{ExecError, LocalExecutor};
use tong_graph::manifest::Manifest;
use tong_graph::{Completed, PlanError, topological_order};
use tong_rust::{RustBackend, capture_system_rust, import_cargo_workspace};
use tong_store::{ActionCache, CachedResult, Cas};

use crate::manifest_mode::manifest_to_model;

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
    let store = tong_dir.join("store");
    let exec = tong_dir.join("exec");
    let cas = Cas::open(&store)?;
    let cache = ActionCache::open(&cas)?;

    // Toolchain first: needed by the backend for action identity.
    let toolchain = capture_system_rust(&cas)?;

    // Load the model: native Tong.toml or Cargo.toml import.
    let model = load_model(root)?;

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
    let mut outcome = BuildOutcome {
        actions_total: order.len(),
        ..Default::default()
    };

    for (index, action) in order.iter().enumerate() {
        let spec = (action.make)(&completed, &cas)?;
        let digest = spec.digest();
        if let Some(result) = cache.get(digest)? {
            outcome.actions_cached += 1;
            println!(
                "  [{}/{}] {} ({}) [cached]",
                index + 1,
                order.len(),
                spec.logical_id.0,
                spec.mnemonic
            );
            completed.0.insert(spec.logical_id.clone(), result);
            continue;
        }

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
        completed.0.insert(spec.logical_id.clone(), cached);
        outcome.actions_executed += 1;
    }

    // Assemble requested final artifacts.
    let out_dir = tong_dir.join("out").join(&options.profile);
    let requested: Vec<&tong_rust::FinalArtifact> = artifacts
        .iter()
        .filter(|artifact| {
            options.targets.is_empty() || options.targets.iter().any(|t| t == &artifact.name)
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
            if fs::hard_link(&blob_path, &target).is_err() {
                fs::copy(&blob_path, &target)?;
            }
        }
        outcome.artifacts.push(dest);
    }

    Ok(outcome)
}

/// Loads the Rust model: `Tong.toml` if present, else Cargo import.
pub fn load_model(root: &Path) -> Result<tong_rust::RustModel, BuildError> {
    if root.join("Tong.toml").exists() {
        let manifest = Manifest::load(root).map_err(|err| BuildError::Manifest(err.to_string()))?;
        Ok(manifest_to_model(&manifest, root))
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

/// Removes the project-local `.tong` directory.
pub fn clean(root: &Path) -> io::Result<()> {
    let tong_dir = root.join(".tong");
    if tong_dir.exists() {
        fs::remove_dir_all(tong_dir)?;
    }
    Ok(())
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
