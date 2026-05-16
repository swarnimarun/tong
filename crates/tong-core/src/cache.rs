use crate::action::Action;
use crate::error::{IoContext, Result};
use crate::hash;
use std::collections::BTreeMap;
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

    pub fn stamp_root(&self) -> &Path {
        &self.root
    }

    pub fn lookup(&self, key: &str, action: &Action) -> CacheStatus {
        let stamp_path = self.stamp_path(key);
        if !stamp_path.exists() {
            return CacheStatus::Miss;
        }

        let stamp_text = match fs::read_to_string(&stamp_path) {
            Ok(s) => s,
            Err(_) => return CacheStatus::Miss,
        };

        let entries = parse_stamp_entries(&stamp_text);

        for output in &action.outputs {
            let output_path = display_path_output(output);
            if !output.exists() {
                return CacheStatus::Miss;
            }
            if let Some(expected_hash) = entries.get(&output_path) {
                let current_hash = match hash::hash_file(output) {
                    Ok(h) => h,
                    Err(_) => return CacheStatus::Miss,
                };
                if current_hash.as_str() != expected_hash.as_str() {
                    return CacheStatus::Miss;
                }
            } else {
                return CacheStatus::Miss;
            }
        }

        if let Some(stdout) = &action.stdout {
            let stdout_path = display_path_output(stdout);
            if !stdout.exists() {
                return CacheStatus::Miss;
            }
            if let Some(expected_hash) = entries.get(&stdout_path) {
                let current_hash = match hash::hash_file(stdout) {
                    Ok(h) => h,
                    Err(_) => return CacheStatus::Miss,
                };
                if current_hash.as_str() != expected_hash.as_str() {
                    return CacheStatus::Miss;
                }
            } else {
                return CacheStatus::Miss;
            }
        }

        CacheStatus::Hit
    }

    pub fn store(&self, key: &str, action: &Action) -> Result<()> {
        fs::create_dir_all(&self.root).with_context(format!(
            "failed to create action cache {}",
            self.root.display()
        ))?;
        let mut stamp = String::new();
        stamp.push_str("version = 2\n");
        stamp.push_str(&format!("key = \"{key}\"\n"));
        stamp.push_str(&format!("action = \"{}\"\n", action.id));

        for output in &action.outputs {
            let output_hash = hash::hash_file(output).unwrap_or_default();
            stamp.push_str(&format!(
                "output = {}:{}\n",
                display_path_output(output),
                output_hash
            ));
        }

        if let Some(stdout) = &action.stdout {
            let stdout_hash = hash::hash_file(stdout).unwrap_or_default();
            stamp.push_str(&format!(
                "stdout = {}:{}\n",
                display_path_output(stdout),
                stdout_hash
            ));
        }

        fs::write(self.stamp_path(key), stamp)
            .with_context(format!("failed to write action cache stamp for {key}"))
    }

    fn stamp_path(&self, key: &str) -> PathBuf {
        self.root.join(format!("{key}.stamp"))
    }
}

fn parse_stamp_entries(text: &str) -> BTreeMap<String, String> {
    let mut entries = BTreeMap::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("output = ") {
            if let Some((path, hash)) = rest.split_once(':') {
                entries.insert(path.to_owned(), hash.to_owned());
            }
        } else if let Some(rest) = line.strip_prefix("stdout = ")
            && let Some((path, hash)) = rest.split_once(':')
        {
            entries.insert(path.to_owned(), hash.to_owned());
        }
    }
    entries
}

pub fn ensure_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(format!("failed to create directory {}", parent.display()))?;
    }
    Ok(())
}

fn display_path_output(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}
