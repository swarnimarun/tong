use crate::action::Action;
use crate::error::{IoContext, Result};
use crate::hash::hash_file;
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
        let stamp_path = self.stamp_path(key);
        if !stamp_path.exists() {
            return CacheStatus::Miss;
        }

        let stamp = match fs::read_to_string(&stamp_path)
            .with_context(format!("failed to read stamp {}", stamp_path.display()))
        {
            Ok(s) => s,
            Err(_) => return CacheStatus::Miss,
        };

        let mut expected_outputs: Vec<(String, String)> = Vec::new();
        let mut expected_stdout: Option<String> = None;

        for line in stamp.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(rest) = line.strip_prefix("output ") {
                if let Some((path, hash)) = rest.split_once(' ') {
                    expected_outputs.push((path.to_owned(), hash.to_owned()));
                }
            } else if let Some(rest) = line.strip_prefix("stdout ") {
                expected_stdout = Some(rest.to_owned());
            }
        }

        for output in &action.outputs {
            if output.is_dir() {
                continue;
            }
            if !output.exists() {
                return CacheStatus::Miss;
            }
            let rel = relative_path(&self.root, output);
            let expected = expected_outputs
                .iter()
                .find(|(p, _)| p == &rel)
                .map(|(_, h)| h.as_str());
            if expected.is_none() {
                return CacheStatus::Miss;
            }
            let actual = match hash_file(output) {
                Ok(h) => h,
                Err(_) => return CacheStatus::Miss,
            };
            if expected != Some(&actual) {
                return CacheStatus::Miss;
            }
        }

        if let Some(stdout) = &action.stdout {
            if !stdout.exists() {
                return CacheStatus::Miss;
            }
            let expected = match expected_stdout {
                Some(h) => h,
                None => return CacheStatus::Miss,
            };
            let actual = match hash_file(stdout) {
                Ok(h) => h,
                Err(_) => return CacheStatus::Miss,
            };
            if expected != actual {
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
        stamp.push_str("# tong action stamp v2\n");
        stamp.push_str(&format!("key = \"{key}\"\n"));
        stamp.push_str(&format!("action = \"{}\"\n", action.id));

        for output in &action.outputs {
            if output.is_dir() {
                continue;
            }
            let hash = hash_file(output)?;
            let rel = relative_path(&self.root, output);
            stamp.push_str(&format!("output {rel} {hash}\n"));
        }

        if let Some(stdout) = &action.stdout
            && stdout.is_file()
        {
            let hash = hash_file(stdout)?;
            stamp.push_str(&format!("stdout {hash}\n"));
        }

        fs::write(self.stamp_path(key), stamp)
            .with_context(format!("failed to write action cache stamp for {key}"))
    }

    pub fn stamp_path(&self, key: &str) -> PathBuf {
        self.root.join(format!("{key}.stamp"))
    }
}

fn relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| path.display().to_string())
}

pub fn ensure_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(format!("failed to create directory {}", parent.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::Action;
    use std::collections::BTreeMap;

    fn test_action(outputs: Vec<PathBuf>, stdout: Option<PathBuf>) -> Action {
        Action {
            id: "test".to_owned(),
            mnemonic: "Test".to_owned(),
            program: PathBuf::from("/bin/true"),
            args: Vec::new(),
            env_bundle: None,
            env: BTreeMap::new(),
            inputs: Vec::new(),
            outputs,
            workdir: PathBuf::from("."),
            key_material: BTreeMap::new(),
            stdout,
        }
    }

    #[test]
    fn cache_miss_when_stamp_missing() {
        let root = std::env::temp_dir().join(format!(
            "tong-cache-miss-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let cache = ActionCache::new(root.clone());
        let action = test_action(vec![root.join("out.txt")], None);
        assert_eq!(cache.lookup("missing", &action), CacheStatus::Miss);
    }

    #[test]
    fn cache_hit_when_outputs_match() {
        let root = std::env::temp_dir().join(format!(
            "tong-cache-hit-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let output = root.join("out.txt");
        fs::write(&output, "hello").unwrap();

        let cache = ActionCache::new(root.clone());
        let action = test_action(vec![output.clone()], None);
        cache.store("test-key", &action).unwrap();

        assert_eq!(cache.lookup("test-key", &action), CacheStatus::Hit);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cache_miss_when_output_changed() {
        let root = std::env::temp_dir().join(format!(
            "tong-cache-change-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let output = root.join("out.txt");
        fs::write(&output, "hello").unwrap();

        let cache = ActionCache::new(root.clone());
        let action = test_action(vec![output.clone()], None);
        cache.store("test-key", &action).unwrap();

        fs::write(&output, "world").unwrap();
        assert_eq!(cache.lookup("test-key", &action), CacheStatus::Miss);

        fs::remove_dir_all(root).unwrap();
    }
}
