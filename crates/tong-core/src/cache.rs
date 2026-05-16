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

    pub fn lookup(&self, key: &str, action: &Action) -> CacheStatus {
        let stamp = self.stamp_path(key);
        if !stamp.exists() {
            return CacheStatus::Miss;
        }

        let stamp_content = match fs::read_to_string(&stamp) {
            Ok(content) => content,
            Err(_) => return CacheStatus::Miss,
        };

        let expected_hashes = parse_stamp_hashes(&stamp_content);

        for output in &action.outputs {
            if !output.exists() {
                return CacheStatus::Miss;
            }
            let path_key = display_path_for_stamp(output);
            if let Some(expected) = expected_hashes.get(&path_key) {
                match hash::hash_file(output) {
                    Ok(actual) if &actual == expected => {}
                    _ => return CacheStatus::Miss,
                }
            }
        }

        if let Some(stdout) = &action.stdout {
            if !stdout.exists() {
                return CacheStatus::Miss;
            }
            if let Some(expected) = expected_hashes.get("__stdout__") {
                match hash::hash_file(stdout) {
                    Ok(actual) if &actual == expected => {}
                    _ => return CacheStatus::Miss,
                }
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
            if output.exists()
                && let Ok(h) = hash::hash_file(output)
            {
                stamp.push_str(&format!(
                    "output.\"{}\" = \"{h}\"\n",
                    display_path_for_stamp(output)
                ));
            }
        }

        if let Some(stdout) = &action.stdout
            && stdout.exists()
            && let Ok(h) = hash::hash_file(stdout)
        {
            stamp.push_str(&format!("stdout = \"{h}\"\n"));
        }

        fs::write(self.stamp_path(key), stamp)
            .with_context(format!("failed to write action cache stamp for {key}"))
    }

    fn stamp_path(&self, key: &str) -> PathBuf {
        self.root.join(format!("{key}.stamp"))
    }

    pub fn stamp_path_for(&self, key: &str) -> PathBuf {
        self.stamp_path(key)
    }
}

fn display_path_for_stamp(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn parse_stamp_hashes(content: &str) -> BTreeMap<String, String> {
    let mut hashes = BTreeMap::new();
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("output.\"")
            && let Some((path, hash)) = rest.split_once("\" = \"")
            && let Some(hash) = hash.strip_suffix('"')
        {
            hashes.insert(path.to_owned(), hash.to_owned());
        } else if let Some(hash) = line.strip_prefix("stdout = \"")
            && let Some(hash) = hash.strip_suffix('"')
        {
            hashes.insert("__stdout__".to_owned(), hash.to_owned());
        }
    }
    hashes
}

pub fn ensure_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(format!("failed to create directory {}", parent.display()))?;
    }
    Ok(())
}
