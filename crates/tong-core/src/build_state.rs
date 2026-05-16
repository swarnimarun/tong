use crate::error::{IoContext, Result, TongError};
use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct BuildState {
    pub root: PathBuf,
    live_paths: BTreeSet<PathBuf>,
}

impl BuildState {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            live_paths: BTreeSet::new(),
        }
    }

    pub fn mark(&mut self, path: impl AsRef<Path>) {
        self.live_paths.insert(path.as_ref().to_path_buf());
    }

    pub fn mark_many<'a>(&mut self, paths: impl IntoIterator<Item = &'a PathBuf>) {
        for path in paths {
            self.mark(path);
        }
    }

    pub fn write(&self) -> Result<()> {
        fs::create_dir_all(&self.root).with_context(format!(
            "failed to create build state root {}",
            self.root.display()
        ))?;
        let mut raw = String::new();
        raw.push_str("version = 1\n");
        for path in &self.live_paths {
            raw.push_str("live = ");
            raw.push_str(&path.to_string_lossy());
            raw.push('\n');
        }
        fs::write(self.path(), raw).with_context(format!(
            "failed to write build state {}",
            self.path().display()
        ))
    }

    pub fn read(root: PathBuf) -> Result<Self> {
        let path = root.join("build-state");
        let raw = fs::read_to_string(&path)
            .with_context(format!("failed to read build state {}", path.display()))?;
        let mut state = Self::new(root);
        for line in raw.lines() {
            if let Some(path) = line.strip_prefix("live = ") {
                state.mark(PathBuf::from(path));
            }
        }
        if state.live_paths.is_empty() {
            return Err(TongError::unsupported(
                "build state is empty; refusing to garbage collect target/tong",
            ));
        }
        Ok(state)
    }

    pub fn gc(&self) -> Result<usize> {
        if !self.root.exists() {
            return Ok(0);
        }
        let mut removed = 0;
        let live = self.live_paths.iter().collect::<BTreeSet<_>>();
        gc_inner(&self.root, &self.root, &live, &mut removed)?;
        Ok(removed)
    }

    pub fn path(&self) -> PathBuf {
        self.root.join("build-state")
    }
}

pub fn begin(root: &Path) -> Result<()> {
    fs::create_dir_all(root).with_context(format!(
        "failed to create build state root {}",
        root.display()
    ))?;
    fs::write(root.join("build-state"), "version = 1\n").with_context(format!(
        "failed to write build state {}",
        root.join("build-state").display()
    ))
}

pub fn record_action(
    root: &Path,
    outputs: &[PathBuf],
    stdout: Option<&PathBuf>,
    stamp: &Path,
) -> Result<()> {
    let mut paths = outputs.to_vec();
    if let Some(stdout) = stdout {
        paths.push(stdout.clone());
    }
    paths.push(stamp.to_path_buf());
    record_paths(root, paths.iter())
}

pub fn record_paths<'a>(root: &Path, paths: impl IntoIterator<Item = &'a PathBuf>) -> Result<()> {
    let path = root.join("build-state");
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(format!("failed to open build state {}", path.display()))?;
    for live_path in paths {
        writeln!(file, "live = {}", live_path.display())
            .with_context(format!("failed to write build state {}", path.display()))?;
    }
    Ok(())
}

fn gc_inner(
    root: &Path,
    path: &Path,
    live: &BTreeSet<&PathBuf>,
    removed: &mut usize,
) -> Result<bool> {
    if path.file_name().and_then(|name| name.to_str()) == Some("build-state") {
        return Ok(false);
    }
    if live.iter().any(|live_path| live_path.as_path() == path) {
        return Ok(false);
    }
    if path.is_dir() {
        let mut entries = fs::read_dir(path)
            .with_context(format!("failed to read {}", path.display()))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .with_context(format!("failed to read {}", path.display()))?;
        entries.sort_by_key(|entry| entry.path());
        for entry in entries {
            let child = entry.path();
            if gc_inner(root, &child, live, removed)? {
                if child.is_dir() {
                    fs::remove_dir(&child)
                        .with_context(format!("failed to remove {}", child.display()))?;
                } else {
                    fs::remove_file(&child)
                        .with_context(format!("failed to remove {}", child.display()))?;
                }
                *removed += 1;
            }
        }
        Ok(path != root
            && fs::read_dir(path)
                .map(|mut it| it.next().is_none())
                .unwrap_or(false))
    } else {
        Ok(true)
    }
}
