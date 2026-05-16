use crate::action::Action;
use crate::cache::{ActionCache, CacheStatus, ensure_parent};
use crate::error::{IoContext, Result, TongError};
use std::fs;
use std::process::{Command, Stdio};

#[derive(Debug, Clone)]
pub struct Executor {
    pub cache: ActionCache,
    pub workspace_root: std::path::PathBuf,
    pub verbose: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionRunStatus {
    Cached,
    Executed,
}

impl Executor {
    pub fn run(&self, action: &Action) -> Result<ActionRunStatus> {
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

        let mut command = Command::new(&action.program);
        command
            .args(&action.args)
            .current_dir(&action.workdir)
            .env_clear()
            .envs(&action.env)
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
        Ok(ActionRunStatus::Executed)
    }
}
