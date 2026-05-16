use crate::action::Action;
use crate::error::{IoContext, Result};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct ActionCache {
    root: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheStatus {
    Hit,
    Miss,
}

impl ActionCache {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn lookup(&self, key: &str, action: &Action) -> CacheStatus {
        if !self.stamp_path(key).exists() {
            return CacheStatus::Miss;
        }
        if action.outputs.iter().all(|output| output.exists()) {
            if action.stdout.as_ref().is_none_or(|stdout| stdout.exists()) {
                CacheStatus::Hit
            } else {
                CacheStatus::Miss
            }
        } else {
            CacheStatus::Miss
        }
    }

    pub fn store(&self, key: &str, action: &Action) -> Result<()> {
        fs::create_dir_all(&self.root).with_context(format!(
            "failed to create action cache {}",
            self.root.display()
        ))?;
        let mut stamp = String::new();
        stamp.push_str("version = 1\n");
        stamp.push_str(&format!("key = \"{key}\"\n"));
        stamp.push_str(&format!("action = \"{}\"\n", action.id));
        fs::write(self.stamp_path(key), stamp)
            .with_context(format!("failed to write action cache stamp for {key}"))
    }

    fn stamp_path(&self, key: &str) -> PathBuf {
        self.root.join(format!("{key}.stamp"))
    }
}

pub fn ensure_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(format!("failed to create directory {}", parent.display()))?;
    }
    Ok(())
}
