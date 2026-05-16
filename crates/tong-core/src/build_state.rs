use crate::error::{IoContext, Result};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

const STATE_FILE: &str = "build-state";

#[derive(Debug, Clone, Default)]
pub struct BuildState {
    pub outputs: BTreeSet<PathBuf>,
    pub stdout_files: BTreeSet<PathBuf>,
    pub cache_stamps: BTreeSet<PathBuf>,
    pub dep_info_files: BTreeSet<PathBuf>,
    pub materialized_roots: BTreeSet<PathBuf>,
}

impl BuildState {
    pub fn load(tong_root: &Path) -> Result<Self> {
        let path = tong_root.join(STATE_FILE);
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = fs::read_to_string(&path)
            .with_context(format!("failed to read build state {}", path.display()))?;
        parse_state(&content)
    }

    pub fn save(&self, tong_root: &Path) -> Result<()> {
        let path = tong_root.join(STATE_FILE);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(format!("failed to create directory {}", parent.display()))?;
        }
        let content = serialize_state(self);
        fs::write(&path, content)
            .with_context(format!("failed to write build state {}", path.display()))
    }

    pub fn record_output(&mut self, path: PathBuf) {
        self.outputs.insert(path);
    }

    pub fn record_stdout(&mut self, path: PathBuf) {
        self.stdout_files.insert(path);
    }

    pub fn record_cache_stamp(&mut self, path: PathBuf) {
        self.cache_stamps.insert(path);
    }

    pub fn record_dep_info(&mut self, path: PathBuf) {
        self.dep_info_files.insert(path);
    }

    pub fn record_materialized_root(&mut self, path: PathBuf) {
        self.materialized_roots.insert(path);
    }

    pub fn live_paths(&self) -> BTreeSet<PathBuf> {
        let mut live = BTreeSet::new();
        live.extend(self.outputs.iter().cloned());
        live.extend(self.stdout_files.iter().cloned());
        live.extend(self.cache_stamps.iter().cloned());
        live.extend(self.dep_info_files.iter().cloned());
        live.extend(self.materialized_roots.iter().cloned());
        live
    }
}

fn parse_state(content: &str) -> Result<BuildState> {
    let mut state = BuildState::default();
    let mut current_section: Option<&str> = None;

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if let Some(section_name) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            current_section = Some(section_name);
            continue;
        }

        if let Some(path) = line.strip_prefix("- ") {
            match current_section {
                Some("outputs") => {
                    state.outputs.insert(PathBuf::from(path));
                }
                Some("stdout") => {
                    state.stdout_files.insert(PathBuf::from(path));
                }
                Some("stamps") => {
                    state.cache_stamps.insert(PathBuf::from(path));
                }
                Some("dep-info") => {
                    state.dep_info_files.insert(PathBuf::from(path));
                }
                Some("materialized") => {
                    state.materialized_roots.insert(PathBuf::from(path));
                }
                _ => {}
            }
        }
    }

    Ok(state)
}

fn serialize_state(state: &BuildState) -> String {
    let mut content = String::new();
    content.push_str("# Tong build state\n");

    content.push_str("[outputs]\n");
    for path in &state.outputs {
        content.push_str(&format!("- {}\n", path.display()));
    }

    content.push_str("[stdout]\n");
    for path in &state.stdout_files {
        content.push_str(&format!("- {}\n", path.display()));
    }

    content.push_str("[stamps]\n");
    for path in &state.cache_stamps {
        content.push_str(&format!("- {}\n", path.display()));
    }

    content.push_str("[dep-info]\n");
    for path in &state.dep_info_files {
        content.push_str(&format!("- {}\n", path.display()));
    }

    content.push_str("[materialized]\n");
    for path in &state.materialized_roots {
        content.push_str(&format!("- {}\n", path.display()));
    }

    content
}

pub fn collect_all_under(dir: &Path) -> Result<BTreeSet<PathBuf>> {
    let mut paths = BTreeSet::new();
    if !dir.exists() {
        return Ok(paths);
    }
    collect_all_recursive(dir, &mut paths)?;
    Ok(paths)
}

fn collect_all_recursive(dir: &Path, paths: &mut BTreeSet<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(format!("failed to read {}", dir.display()))? {
        let entry = entry.with_context(format!("failed to read entry in {}", dir.display()))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .with_context(format!("failed to inspect {}", path.display()))?;
        if file_type.is_dir() {
            collect_all_recursive(&path, paths)?;
        } else {
            paths.insert(path);
        }
    }
    Ok(())
}
