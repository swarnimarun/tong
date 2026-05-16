use crate::error::{IoContext, Result};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default)]
pub struct BuildState {
    pub outputs: Vec<PathBuf>,
    pub stdouts: Vec<PathBuf>,
    pub stamps: Vec<PathBuf>,
    pub dep_infos: Vec<PathBuf>,
    pub sources: Vec<PathBuf>,
}

impl BuildState {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }

        let raw = fs::read_to_string(path)
            .with_context(format!("failed to read build state {}", path.display()))?;

        let mut state = Self::default();
        for line in raw.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Some(rest) = line.strip_prefix("output ") {
                state.outputs.push(PathBuf::from(rest));
            } else if let Some(rest) = line.strip_prefix("stdout ") {
                state.stdouts.push(PathBuf::from(rest));
            } else if let Some(rest) = line.strip_prefix("stamp ") {
                state.stamps.push(PathBuf::from(rest));
            } else if let Some(rest) = line.strip_prefix("depinfo ") {
                state.dep_infos.push(PathBuf::from(rest));
            } else if let Some(rest) = line.strip_prefix("source ") {
                state.sources.push(PathBuf::from(rest));
            }
        }

        Ok(state)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(format!("failed to create directory {}", parent.display()))?;
        }

        let mut lines = Vec::new();
        for output in &self.outputs {
            lines.push(format!("output {}", output.display()));
        }
        for stdout in &self.stdouts {
            lines.push(format!("stdout {}", stdout.display()));
        }
        for stamp in &self.stamps {
            lines.push(format!("stamp {}", stamp.display()));
        }
        for depinfo in &self.dep_infos {
            lines.push(format!("depinfo {}", depinfo.display()));
        }
        for source in &self.sources {
            lines.push(format!("source {}", source.display()));
        }

        fs::write(path, lines.join("\n"))
            .with_context(format!("failed to write build state {}", path.display()))?;
        Ok(())
    }

    pub fn all_live_paths(&self) -> BTreeSet<PathBuf> {
        let mut set = BTreeSet::new();
        set.extend(self.outputs.iter().cloned());
        set.extend(self.stdouts.iter().cloned());
        set.extend(self.stamps.iter().cloned());
        set.extend(self.dep_infos.iter().cloned());
        set.extend(self.sources.iter().cloned());
        set
    }
}

pub fn gc(root: &Path, dry_run: bool) -> Result<(usize, usize)> {
    let state_path = root.join("build-state");
    if !state_path.exists() {
        return Err(crate::error::TongError::unsupported(
            "no build state found; run `tong clean` for a full reset",
        ));
    }

    let state = BuildState::load(&state_path)?;
    let live = state.all_live_paths();

    let mut scanned = 0usize;
    let mut removed = 0usize;

    fn scan_dir(
        dir: &Path,
        live: &BTreeSet<PathBuf>,
        dry_run: bool,
        scanned: &mut usize,
        removed: &mut usize,
    ) -> Result<()> {
        if !dir.exists() {
            return Ok(());
        }

        let entries = fs::read_dir(dir)
            .with_context(format!("failed to read directory {}", dir.display()))?;

        for entry in entries {
            let entry = entry.with_context(format!("failed to read entry in {}", dir.display()))?;
            let path = entry.path();
            *scanned += 1;

            if path.is_dir() {
                scan_dir(&path, live, dry_run, scanned, removed)?;
                // Remove empty directories
                if let Ok(mut contents) = fs::read_dir(&path)
                    && contents.next().is_none()
                    && !live.contains(&path)
                {
                    if !dry_run {
                        fs::remove_dir(&path)
                            .with_context(format!("failed to remove {}", path.display()))?;
                    }
                    *removed += 1;
                }
            } else if !live.contains(&path) {
                if !dry_run {
                    fs::remove_file(&path)
                        .with_context(format!("failed to remove {}", path.display()))?;
                }
                *removed += 1;
            }
        }

        Ok(())
    }

    scan_dir(root, &live, dry_run, &mut scanned, &mut removed)?;

    Ok((scanned, removed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_build_state() {
        let root = std::env::temp_dir().join(format!(
            "tong-state-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("build-state");

        let state = BuildState {
            outputs: vec![PathBuf::from("target/tong/debug/bin/app")],
            stdouts: vec![PathBuf::from("target/tong/debug/build/out")],
            stamps: vec![PathBuf::from("target/tong/cache/actions/abc.stamp")],
            dep_infos: vec![],
            sources: vec![PathBuf::from("target/tong/store/sources/src")],
        };

        state.save(&path).unwrap();
        let loaded = BuildState::load(&path).unwrap();
        assert_eq!(loaded.outputs, state.outputs);
        assert_eq!(loaded.stamps, state.stamps);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn gc_removes_untracked_files() {
        let root = std::env::temp_dir().join(format!(
            "tong-gc-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();

        let live_file = root.join("live.txt");
        let dead_file = root.join("dead.txt");
        fs::write(&live_file, "live").unwrap();
        fs::write(&dead_file, "dead").unwrap();

        let state_path = root.join("build-state");
        let state = BuildState {
            outputs: vec![live_file.clone(), state_path.clone()],
            ..Default::default()
        };
        state.save(&state_path).unwrap();

        let (_, removed) = gc(&root, false).unwrap();
        assert!(live_file.exists());
        assert!(!dead_file.exists());
        assert_eq!(removed, 1);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn gc_refuses_without_state() {
        let root = std::env::temp_dir().join(format!(
            "tong-gc-nostate-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        assert!(gc(&root, false).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
