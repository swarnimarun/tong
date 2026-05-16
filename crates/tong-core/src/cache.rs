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
        let stamp_path = self.stamp_path(key);
        if !stamp_path.exists() {
            return CacheStatus::Miss;
        }
        let Ok(stamp) = CacheStamp::read(&stamp_path) else {
            return CacheStatus::Miss;
        };
        if stamp.key != key || stamp.action != action.id {
            return CacheStatus::Miss;
        }
        for output in action.outputs.iter().chain(action.stdout.iter()) {
            if !output.exists() {
                return CacheStatus::Miss;
            }
            let Ok(hash) = hash_output(output) else {
                return CacheStatus::Miss;
            };
            if stamp.outputs.get(output) != Some(&hash) {
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
        stamp.push_str("version = 1\n");
        stamp.push_str(&format!("key = \"{key}\"\n"));
        stamp.push_str(&format!("action = \"{}\"\n", action.id));
        for output in action.outputs.iter().chain(action.stdout.iter()) {
            let hash = hash_output(output)?;
            stamp.push_str(&format!(
                "output = \"{} {}\"\n",
                escape_field(&output.to_string_lossy()),
                hash
            ));
        }
        fs::write(self.stamp_path(key), stamp)
            .with_context(format!("failed to write action cache stamp for {key}"))
    }

    pub fn stamp_path(&self, key: &str) -> PathBuf {
        self.root.join(format!("{key}.stamp"))
    }
}

#[derive(Debug)]
struct CacheStamp {
    key: String,
    action: String,
    outputs: BTreeMap<PathBuf, String>,
}

impl CacheStamp {
    fn read(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path).with_context(format!(
            "failed to read action cache stamp {}",
            path.display()
        ))?;
        let mut key = String::new();
        let mut action = String::new();
        let mut outputs = BTreeMap::new();
        for line in raw.lines() {
            let Some((name, value)) = line.split_once(" = ") else {
                continue;
            };
            let value = value.trim_matches('"');
            match name {
                "key" => key = value.to_owned(),
                "action" => action = value.to_owned(),
                "output" => {
                    if let Some((path, hash)) = value.rsplit_once(' ') {
                        outputs.insert(PathBuf::from(unescape_field(path)), hash.to_owned());
                    }
                }
                _ => {}
            }
        }
        Ok(Self {
            key,
            action,
            outputs,
        })
    }
}

fn hash_output(path: &Path) -> Result<String> {
    if path.is_dir() {
        let mut hasher = hash::StableHasher::new();
        hash_dir(path, path, &mut hasher)?;
        Ok(hasher.finish_hex())
    } else {
        hash::hash_file(path)
    }
}

fn hash_dir(root: &Path, path: &Path, hasher: &mut hash::StableHasher) -> Result<()> {
    let mut entries = fs::read_dir(path)
        .with_context(format!(
            "failed to read output directory {}",
            path.display()
        ))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(format!(
            "failed to read output directory {}",
            path.display()
        ))?;
    entries.sort_by_key(|entry| entry.path());
    for entry in entries {
        let path = entry.path();
        let relative = path.strip_prefix(root).unwrap_or(&path);
        hasher.update_str(&relative.to_string_lossy());
        if path.is_dir() {
            hasher.update_str("dir");
            hash_dir(root, &path, hasher)?;
        } else {
            hasher.update_str("file");
            hasher.update_str(&hash::hash_file(&path)?);
        }
    }
    Ok(())
}

fn escape_field(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn unescape_field(value: &str) -> String {
    let mut out = String::new();
    let mut escaped = false;
    for ch in value.chars() {
        if escaped {
            out.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else {
            out.push(ch);
        }
    }
    out
}

pub fn ensure_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(format!("failed to create directory {}", parent.display()))?;
    }
    Ok(())
}
