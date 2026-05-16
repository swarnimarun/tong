use crate::action::Action;
use crate::build_state::BuildState;
use crate::cache::{ActionCache, CacheStatus, ensure_parent};
use crate::error::{IoContext, Result, TongError};
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};

#[derive(Debug, Clone)]
pub struct Executor {
    pub cache: ActionCache,
    pub workspace_root: PathBuf,
    pub verbose: bool,
    pub build_state: BuildState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionRunStatus {
    Cached,
    Executed,
}

impl Executor {
    pub fn run(&mut self, action: &Action) -> Result<ActionRunStatus> {
        let key = action.cache_key(&self.workspace_root)?;
        match self.cache.lookup(&key, action) {
            CacheStatus::Hit => {
                if self.verbose {
                    eprintln!("cached {} {}", action.mnemonic, action.id);
                }
                return Ok(ActionRunStatus::Cached);
            }
            CacheStatus::Miss => {}
        }

        if self.verbose {
            eprintln!("run {} {}", action.mnemonic, action.id);
        }

        for output in &action.outputs {
            ensure_parent(output)?;
            if output.exists() && output.is_file() {
                fs::remove_file(output)
                    .with_context(format!("failed to remove stale {}", output.display()))?;
            }
        }
        if let Some(stdout) = &action.stdout {
            ensure_parent(stdout)?;
        }

        let mut env = action
            .env_bundle
            .as_ref()
            .map(|bundle| bundle.vars.clone())
            .unwrap_or_default();
        env.extend(action.env.clone());

        let mut command = Command::new(&action.program);
        command
            .args(&action.args)
            .current_dir(&action.workdir)
            .env_clear()
            .envs(&env)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let output = command.output().with_context(format!(
            "failed to execute action {} using {}",
            action.id,
            action.program.display()
        ))?;

        if !output.status.success() {
            return Err(TongError::CommandFailed {
                program: action.program.clone(),
                status: output.status.to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }

        if self.verbose {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if !stdout.trim().is_empty() {
                eprint!("{stdout}");
            }
        }

        if let Some(stdout) = &action.stdout {
            fs::write(stdout, &output.stdout).with_context(format!(
                "failed to write action stdout {}",
                stdout.display()
            ))?;
        }

        self.cache.store(&key, action)?;

        for output in &action.outputs {
            self.build_state.outputs.push(output.clone());
        }
        if let Some(stdout) = &action.stdout {
            self.build_state.stdouts.push(stdout.clone());
        }
        self.build_state.stamps.push(self.cache.stamp_path(&key));

        Ok(ActionRunStatus::Executed)
    }

    pub fn save_state(&self, out_dir: &std::path::Path) -> Result<()> {
        let path = out_dir.join("build-state");
        self.build_state.save(&path)
    }
}
