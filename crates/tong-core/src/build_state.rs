use crate::error::{IoContext, Result};
use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct BuildState {
    path: PathBuf,
    live_files: BTreeSet<String>,
}

impl BuildState {
    pub fn new(out_dir: &Path) -> Self {
        Self {
            path: out_dir.join("build-state"),
            live_files: BTreeSet::new(),
        }
    }

    pub fn load(out_dir: &Path) -> Result<Self> {
        let path = out_dir.join("build-state");
        let mut live_files = BTreeSet::new();
        if path.exists() {
            let content = fs::read_to_string(&path)
                .with_context(format!("failed to read build state {}", path.display()))?;
            for line in content.lines() {
                let trimmed = line.trim();
                if !trimmed.is_empty() {
                    live_files.insert(trimmed.to_owned());
                }
            }
        }
        Ok(Self { path, live_files })
    }

    pub fn track_file(&mut self, out_dir: &Path, file: &Path) {
        if let Ok(rel) = file.strip_prefix(out_dir) {
            self.live_files
                .insert(rel.to_string_lossy().replace('\\', "/"));
        }
    }

    pub fn track_dir_recursive(&mut self, out_dir: &Path, dir: &Path) {
        if !dir.exists() {
            return;
        }
        let _ = collect_files_inner(dir, out_dir, &mut self.live_files);
    }

    pub fn write(&self) -> Result<()> {
        let mut content = String::new();
        let mut sorted: Vec<&String> = self.live_files.iter().collect();
        sorted.sort();
        for file in sorted {
            content.push_str(file);
            content.push('\n');
        }
        fs::write(&self.path, &content).with_context(format!(
            "failed to write build state {}",
            self.path.display()
        ))
    }

    pub fn gc(out_dir: &Path) -> Result<usize> {
        let state = Self::load(out_dir)?;
        if state.live_files.is_empty() {
            return Ok(0);
        }

        let mut deletions = 0usize;
        let _ = collect_and_prune(out_dir, out_dir, &state.live_files, &mut deletions);
        state.clear()?;
        Ok(deletions)
    }

    fn clear(&self) -> Result<()> {
        if self.path.exists() {
            fs::remove_file(&self.path).with_context(format!(
                "failed to remove build state {}",
                self.path.display()
            ))?;
        }
        Ok(())
    }
}

fn collect_files_inner(
    dir: &Path,
    out_dir: &Path,
    live_files: &mut BTreeSet<String>,
) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in
        fs::read_dir(dir).with_context(format!("failed to read directory {}", dir.display()))?
    {
        let entry = entry.with_context(format!("failed to read entry in {}", dir.display()))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .with_context(format!("failed to inspect {}", path.display()))?;
        if file_type.is_dir() {
            collect_files_inner(&path, out_dir, live_files)?;
        } else if file_type.is_file()
            && let Ok(rel) = path.strip_prefix(out_dir)
        {
            live_files.insert(rel.to_string_lossy().replace('\\', "/"));
        }
    }
    Ok(())
}

fn collect_and_prune(
    dir: &Path,
    out_dir: &Path,
    live_files: &BTreeSet<String>,
    deletions: &mut usize,
) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in
        fs::read_dir(dir).with_context(format!("failed to read directory {}", dir.display()))?
    {
        let entry = entry.with_context(format!("failed to read entry in {}", dir.display()))?;
        let path = entry.path();

        if let Ok(rel) = path.strip_prefix(out_dir) {
            let rel_str = rel.to_string_lossy().replace('\\', "/");
            if path.is_dir() {
                if rel_str == "build-state" {
                    continue;
                }
                collect_and_prune(&path, out_dir, live_files, deletions)?;
                if is_dir_empty(&path)? {
                    fs::remove_dir(&path).with_context(format!(
                        "failed to remove empty directory {}",
                        path.display()
                    ))?;
                }
            } else if !live_files.contains(&rel_str) {
                fs::remove_file(&path)
                    .with_context(format!("failed to remove dead artifact {}", path.display()))?;
                *deletions += 1;
            }
        }
    }
    Ok(())
}

fn is_dir_empty(dir: &Path) -> Result<bool> {
    if !dir.exists() {
        return Ok(true);
    }
    let mut entries =
        fs::read_dir(dir).with_context(format!("failed to read directory {}", dir.display()))?;
    Ok(entries.next().is_none())
}

pub fn collect_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    if !root.exists() {
        return Ok(files);
    }
    collect_files_recursive(root, &mut files)?;
    Ok(files)
}

fn collect_files_recursive(dir: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in
        fs::read_dir(dir).with_context(format!("failed to read directory {}", dir.display()))?
    {
        let entry = entry.with_context(format!("failed to read entry in {}", dir.display()))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .with_context(format!("failed to inspect {}", path.display()))?;
        if file_type.is_dir() {
            collect_files_recursive(&path, files)?;
        } else if file_type.is_file() {
            files.push(crate::paths::canonicalize(&path)?);
        }
    }
    Ok(())
}

fn _write_and_sync(path: &Path, content: &[u8]) -> Result<()> {
    let mut file =
        fs::File::create(path).with_context(format!("failed to create {}", path.display()))?;
    file.write_all(content)
        .with_context(format!("failed to write {}", path.display()))?;
    file.flush()
        .with_context(format!("failed to flush {}", path.display()))?;
    Ok(())
}
