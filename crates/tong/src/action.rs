use crate::error::Result;
use crate::hash::{StableHasher, hash_file};
use crate::paths;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Action {
    pub id: String,
    pub mnemonic: String,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub inputs: Vec<PathBuf>,
    pub outputs: Vec<PathBuf>,
    pub workdir: PathBuf,
    pub key_material: BTreeMap<String, String>,
    pub stdout: Option<PathBuf>,
}

impl Action {
    pub fn cache_key(&self, workspace_root: &Path) -> Result<String> {
        let mut hasher = StableHasher::new();
        hasher.update_str("tong-action-v1");
        hasher.update_str(&self.id);
        hasher.update_str(&self.mnemonic);
        hasher.update_str(&paths::display_path(&self.program));
        hasher.update_str(&paths::display_path(&self.workdir));

        for arg in &self.args {
            hasher.update_str("arg");
            hasher.update_str(arg);
        }

        for (key, value) in &self.env {
            hasher.update_str("env");
            hasher.update_str(key);
            hasher.update_str(value);
        }

        for (key, value) in &self.key_material {
            hasher.update_str("material");
            hasher.update_str(key);
            hasher.update_str(value);
        }

        let mut inputs = self.inputs.clone();
        inputs.sort();
        for input in inputs {
            hasher.update_str("input");
            hasher.update_str(&relative_or_absolute(workspace_root, &input));
            hasher.update_str(&hash_file(&input)?);
        }

        for output in &self.outputs {
            hasher.update_str("output");
            hasher.update_str(&relative_or_absolute(workspace_root, output));
        }

        if let Some(stdout) = &self.stdout {
            hasher.update_str("stdout");
            hasher.update_str(&relative_or_absolute(workspace_root, stdout));
        }

        Ok(hasher.finish_hex())
    }
}

fn relative_or_absolute(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .map(paths::display_path)
        .unwrap_or_else(|_| paths::display_path(path))
}
