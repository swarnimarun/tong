//! The local process executor.
//!
//! Executes an [`ActionSpec`] on the local machine with a deterministic
//! layout (PLAN.md section 11, enforcement level 1: clean environment):
//!
//! ```text
//! <exec_base>/<invocation-id>/<action-digest>/
//!   in/    materialized input root (sources, dep outputs, tools)
//!   out/   the only directory outputs are captured from
//!   tmp/   TMPDIR and HOME for the action
//!   bin/   materialized executable (when it comes from the store)
//! ```
//!
//! The per-invocation namespace prevents concurrent Tong processes from
//! deleting or rewriting each other's action directories. The action's
//! directory name still derives from its digest, so arguments may reference
//! `{exec_root}` without embedding a logical target name. `{bundle_root}` is
//! substituted for system-captured bundles, whose files are fingerprinted but
//! used in place (non-portable; PLAN.md section 5).
//!
//! Sandboxing beyond a clean environment is Phase 4 work; this executor
//! documents enforcement level 1 and never claims hermeticity.

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tong_core::action::ActionSpec;
use tong_core::artifact::{ArtifactRef, BlobDigest, TreeDigest};
use tong_core::digest::Digest;
use tong_core::paths::{OutputPath, RelativePath};
use tong_core::tree::TreeEntry;
use tong_store::Cas;

use crate::sandbox::{Sandbox, SandboxLevel, SandboxSpec, default_read_only_binds, sandbox_for};

/// Placeholder substituted with the action's exec root path.
pub use tong_core::action::{BUNDLE_ROOT_VAR, EXEC_ROOT_VAR};

static NEXT_EXECUTOR_ID: AtomicU64 = AtomicU64::new(0);

/// The result of a successful, validated execution.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ExecOutcome {
    /// Tree captured from the action's `out/` directory.
    pub outputs: TreeDigest,
    /// Captured stdout.
    pub stdout: BlobDigest,
    /// Captured stderr.
    pub stderr: BlobDigest,
    /// Wall-clock execution time.
    pub duration: Duration,
}

/// Execution failure.
#[derive(Debug)]
pub enum ExecError {
    /// Store or filesystem failure.
    Io(io::Error),
    /// The executable artifact could not be resolved to a file.
    ExecutableMissing(String),
    /// The referenced environment bundle is not in the store.
    BundleMissing(Digest),
    /// The action exceeded its timeout.
    Timeout(Duration),
    /// The process exited non-zero; stderr is available in the store.
    Exit {
        /// Process exit code.
        code: i32,
        /// Captured stderr blob.
        stderr: BlobDigest,
        /// Exec root, kept for debugging.
        exec_root: PathBuf,
    },
    /// A declared output is missing (PLAN.md section 4.6).
    MissingOutput {
        /// The missing declared output.
        path: OutputPath,
        /// Exec root, kept for debugging.
        exec_root: PathBuf,
    },
}

impl fmt::Display for ExecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "I/O error: {err}"),
            Self::ExecutableMissing(what) => write!(f, "cannot resolve executable: {what}"),
            Self::BundleMissing(digest) => write!(f, "environment bundle {digest} not in store"),
            Self::Timeout(limit) => write!(f, "action timed out after {limit:?}"),
            Self::Exit {
                code, exec_root, ..
            } => {
                write!(
                    f,
                    "action exited with code {code} (exec root kept at {})",
                    exec_root.display()
                )
            }
            Self::MissingOutput { path, exec_root } => {
                write!(
                    f,
                    "declared output {path:?} missing (exec root kept at {})",
                    exec_root.display()
                )
            }
        }
    }
}

impl std::error::Error for ExecError {}

impl From<io::Error> for ExecError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

/// A local process executor.
pub struct LocalExecutor {
    cas: Cas,
    invocation_root: PathBuf,
    /// Sandbox enforcement level (PLAN.md section 11; opt-in).
    sandbox_level: SandboxLevel,
    /// The platform sandbox wrapper.
    sandbox: Box<dyn Sandbox>,
    /// System-captured executables: blob digest → real local path.
    system_tools: HashMap<Digest, PathBuf>,
    /// System-captured bundle roots: bundle digest → local directory.
    bundle_roots: HashMap<Digest, PathBuf>,
    /// Keep exec roots after successful runs (debugging).
    keep_exec_roots: bool,
}

impl LocalExecutor {
    /// Creates an executor using `exec_base` for working directories.
    pub fn new(cas: Cas, exec_base: impl Into<PathBuf>) -> io::Result<Self> {
        Self::with_sandbox(cas, exec_base, SandboxLevel::L1)
    }

    /// Creates an executor with a sandbox enforcement level.
    pub fn with_sandbox(
        cas: Cas,
        exec_base: impl Into<PathBuf>,
        sandbox_level: SandboxLevel,
    ) -> io::Result<Self> {
        let exec_base = exec_base.into();
        fs::create_dir_all(&exec_base)?;
        let executor_id = NEXT_EXECUTOR_ID.fetch_add(1, Ordering::Relaxed);
        let invocation_root = exec_base.join(format!("run-{}-{executor_id}", std::process::id()));
        fs::create_dir_all(&invocation_root)?;
        let host = std::env::consts::OS;
        let sandbox = sandbox_for(host);
        Ok(Self {
            cas,
            invocation_root,
            sandbox_level,
            sandbox,
            system_tools: HashMap::new(),
            bundle_roots: HashMap::new(),
            keep_exec_roots: false,
        })
    }

    /// Registers a system-captured executable (PLAN.md section 5: local-only
    /// compatibility; the action records the binary's digest, execution uses
    /// the real path so the toolchain's sibling libraries resolve).
    pub fn register_system_tool(&mut self, blob: BlobDigest, path: PathBuf) {
        self.system_tools.insert(blob.digest(), path);
    }

    /// Registers the local root of a system-captured bundle.
    pub fn register_bundle_root(&mut self, bundle: Digest, root: PathBuf) {
        self.bundle_roots.insert(bundle, root);
    }

    /// Keeps exec roots after successful runs (for debugging).
    pub fn set_keep_exec_roots(&mut self, keep: bool) {
        self.keep_exec_roots = keep;
    }

    /// Executes an action. Cache lookup happens in the driver, not here.
    pub fn execute(&self, spec: &ActionSpec) -> Result<ExecOutcome, ExecError> {
        let digest = spec.digest();
        let exec_root = self.invocation_root.join(digest.to_hex());
        let started = Instant::now();

        // Recreate only this invocation's action directory. Another build may
        // execute the same digest under its own namespace and atomically race
        // to publish the identical cache result.
        if exec_root.exists() {
            fs::remove_dir_all(&exec_root)?;
        }
        let input = exec_root.join("in");
        let output = exec_root.join("out");
        let tmp = exec_root.join("tmp");
        fs::create_dir_all(&input)?;
        fs::create_dir_all(&output)?;
        fs::create_dir_all(tmp.join("home"))?;

        let result = self.run(spec, &exec_root, &input, &output, &tmp, started);
        if result.is_ok() && !self.keep_exec_roots {
            fs::remove_dir_all(&exec_root)?;
            // Avoid accumulating empty invocation directories. A failed
            // action remains inside the directory for diagnostics.
            let _ = fs::remove_dir(&self.invocation_root);
        }
        result
    }

    fn run(
        &self,
        spec: &ActionSpec,
        exec_root: &Path,
        input: &Path,
        output: &Path,
        tmp: &Path,
        started: Instant,
    ) -> Result<ExecOutcome, ExecError> {
        self.cas.materialize(spec.input_root, input)?;

        let bundle_root = match &spec.environment_bundle {
            Some(reference) => Some(
                self.bundle_roots
                    .get(&reference.digest())
                    .cloned()
                    .ok_or(ExecError::BundleMissing(reference.digest()))?,
            ),
            None => None,
        };
        let substitute = |text: &str| -> String {
            let mut out = text.replace(EXEC_ROOT_VAR, &exec_root.to_string_lossy());
            if let Some(root) = &bundle_root {
                out = out.replace(BUNDLE_ROOT_VAR, &root.to_string_lossy());
            }
            out
        };

        let executable = self.resolve_executable(&spec.executable, exec_root)?;
        let args: Vec<String> = spec
            .arguments
            .iter()
            .map(|arg| substitute(&arg.0))
            .collect();

        let mut env: Vec<(String, String)> = vec![
            (
                "PATH".to_owned(),
                // Platform-provided default; hermetic toolchain PATHs are a
                // Phase 4 deliverable (PLAN.md section 11).
                "/usr/bin:/bin:/usr/sbin:/sbin".to_owned(),
            ),
            ("TMPDIR".to_owned(), tmp.to_string_lossy().into_owned()),
            ("TEMP".to_owned(), tmp.to_string_lossy().into_owned()),
            ("TMP".to_owned(), tmp.to_string_lossy().into_owned()),
            (
                "HOME".to_owned(),
                tmp.join("home").to_string_lossy().into_owned(),
            ),
            ("LC_ALL".to_owned(), "C".to_owned()),
        ];
        if let Some(reference) = &spec.environment_bundle {
            let bundle = self
                .cas
                .get_bundle(reference.digest())?
                .ok_or(ExecError::BundleMissing(reference.digest()))?;
            for (key, value) in &bundle.variables {
                env.push((key.clone(), substitute(value)));
            }
        }
        for (key, value) in &spec.environment {
            env.push((key.clone(), substitute(value)));
        }

        let working_dir = if spec.working_directory.as_str() == RelativePath::ROOT {
            input.to_path_buf()
        } else {
            input.join(spec.working_directory.as_str())
        };

        let stdout_path = tmp.join("stdout");
        let stderr_path = tmp.join("stderr");
        let mut binding = Command::new(&executable);
        let command = binding
            .args(&args)
            .current_dir(&working_dir)
            .env_clear()
            .envs(env.clone())
            .stdin(Stdio::null())
            .stdout(fs::File::create(&stdout_path)?)
            .stderr(fs::File::create(&stderr_path)?);
        let child = command;

        if self.sandbox_level >= SandboxLevel::L3 {
            // Filesystem + network isolation. The captured toolchain's
            // closure (registered bundle roots and system tool parents) is
            // bound read-only so the sandboxed action can still run rustc.
            let mut binds = default_read_only_binds();
            for root in self.bundle_roots.values() {
                binds.push(root.clone());
            }
            for tool in self.system_tools.values() {
                if let Some(parent) = tool.parent() {
                    binds.push(parent.to_path_buf());
                }
            }
            let spec = SandboxSpec {
                level: self.sandbox_level,
                read_only_binds: binds,
                writable: vec![output.to_path_buf(), tmp.to_path_buf()],
                deny_network: spec.network_policy == tong_core::action::NetworkPolicy::Deny,
                environment: env,
            };
            self.sandbox.wrap_command(child, &spec, exec_root)?;
        }

        let mut child = child.spawn().map_err(|err| {
            ExecError::ExecutableMissing(format!("{}: {err}", executable.display()))
        })?;

        let status = wait_with_timeout(&mut child, spec.timeout)?;
        let code = match status {
            Some(status) => status.code().unwrap_or(-1),
            None => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(ExecError::Timeout(spec.timeout.unwrap_or_default()));
            }
        };

        let stdout = self.cas.put_file(&stdout_path)?;
        let stderr = self.cas.put_file(&stderr_path)?;
        if code != 0 {
            return Err(ExecError::Exit {
                code,
                stderr,
                exec_root: exec_root.to_path_buf(),
            });
        }

        // Mandatory output validation before capturing anything (PLAN.md
        // section 4.6): a missing declared output rejects the result.
        for declared in &spec.declared_outputs {
            if !output.join(declared.as_str()).exists() {
                return Err(ExecError::MissingOutput {
                    path: declared.clone(),
                    exec_root: exec_root.to_path_buf(),
                });
            }
        }
        let outputs = self.cas.capture_dir_filtered(output, &Default::default())?;
        Ok(ExecOutcome {
            outputs,
            stdout,
            stderr,
            duration: started.elapsed(),
        })
    }

    /// Resolves an executable artifact to a runnable local path.
    fn resolve_executable(
        &self,
        artifact: &ArtifactRef,
        exec_root: &Path,
    ) -> Result<PathBuf, ExecError> {
        let bin = exec_root.join("bin");
        match artifact {
            ArtifactRef::Blob(digest) => {
                if let Some(path) = self.system_tools.get(&digest.digest()) {
                    return Ok(path.clone());
                }
                let blob = self.cas.blob_path(*digest).ok_or_else(|| {
                    ExecError::ExecutableMissing(format!("blob {}", digest.digest()))
                })?;
                Self::link_executable(&blob, &bin.join("tool"))
            }
            ArtifactRef::TreeFile { tree, path } => {
                let blob = self.find_in_tree(*tree, path)?;
                let local = self.cas.blob_path(blob).ok_or_else(|| {
                    ExecError::ExecutableMissing(format!("blob {}", blob.digest()))
                })?;
                Self::link_executable(&local, &bin.join("tool"))
            }
            ArtifactRef::Tree(tree) => Err(ExecError::ExecutableMissing(format!(
                "tree {} is not a file",
                tree.digest()
            ))),
        }
    }

    fn link_executable(source: &Path, dest: &Path) -> Result<PathBuf, ExecError> {
        fs::create_dir_all(dest.parent().unwrap())?;
        if dest.exists() {
            fs::remove_file(dest)?;
        }
        // Copy, never hard-link: chmodding the exec-root copy through a
        // hard link would mutate the immutable store blob.
        fs::copy(source, dest)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(dest, fs::Permissions::from_mode(0o755))?;
        }
        Ok(dest.to_path_buf())
    }

    /// Walks a tree to the blob at `path`.
    fn find_in_tree(&self, tree: TreeDigest, path: &RelativePath) -> Result<BlobDigest, ExecError> {
        let mut current = tree;
        let components: Vec<&str> = path.as_str().split('/').collect();
        for (index, component) in components.iter().enumerate() {
            let tree_obj = self.cas.get_tree(current)?.ok_or_else(|| {
                ExecError::ExecutableMissing(format!("tree {}", current.digest()))
            })?;
            let entry = tree_obj
                .entries()
                .get(*component)
                .ok_or_else(|| ExecError::ExecutableMissing(format!("{path} not found in tree")))?;
            match (entry, index == components.len() - 1) {
                (TreeEntry::File { digest, .. }, true) => return Ok(*digest),
                (TreeEntry::Directory(sub), false) => current = *sub,
                _ => {
                    return Err(ExecError::ExecutableMissing(format!(
                        "{path} does not name a file"
                    )));
                }
            }
        }
        Err(ExecError::ExecutableMissing(format!(
            "{path} does not name a file"
        )))
    }
}

fn wait_with_timeout(
    child: &mut std::process::Child,
    timeout: Option<Duration>,
) -> io::Result<Option<std::process::ExitStatus>> {
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if let Some(limit) = timeout
            && started.elapsed() > limit
        {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
