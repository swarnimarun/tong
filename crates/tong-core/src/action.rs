use crate::env::EnvBundle;
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
    pub env_bundle: Option<EnvBundle>,
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

        if let Some(bundle) = &self.env_bundle {
            hasher.update_str("env-bundle");
            hasher.update_str(&bundle.id);
            hasher.update_str(bundle.kind.as_str());
            hasher.update_str(&bundle.fingerprint());
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn cache_key_changes_when_input_contents_change() {
        let root = temp_dir("action-cache-key");
        let input = root.join("input.txt");
        fs::create_dir_all(&root).unwrap();
        fs::write(&input, "one").unwrap();

        let action = test_action(&root, &input);
        let first = action.cache_key(&root).unwrap();

        fs::write(&input, "two").unwrap();
        let second = action.cache_key(&root).unwrap();

        assert_ne!(first, second);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cache_key_is_independent_of_input_order() {
        let root = temp_dir("action-input-order");
        fs::create_dir_all(&root).unwrap();
        let first = root.join("a.txt");
        let second = root.join("b.txt");
        fs::write(&first, "a").unwrap();
        fs::write(&second, "b").unwrap();

        let mut left = test_action(&root, &first);
        left.inputs.push(second.clone());
        let mut right = test_action(&root, &second);
        right.inputs.push(first);

        assert_eq!(
            left.cache_key(&root).unwrap(),
            right.cache_key(&root).unwrap()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cache_key_changes_when_env_bundle_changes() {
        let root = temp_dir("action-env-bundle");
        fs::create_dir_all(&root).unwrap();
        let input = root.join("input.txt");
        fs::write(&input, "input").unwrap();

        let mut left = test_action(&root, &input);
        left.env_bundle = Some(crate::env::EnvBundle::new(
            "demo",
            crate::env::EnvBundleKind::HostToolchain,
        ));

        let mut right = test_action(&root, &input);
        let mut bundle =
            crate::env::EnvBundle::new("demo", crate::env::EnvBundleKind::HostToolchain);
        bundle
            .vars
            .insert("PATH".to_owned(), "/toolchain/bin".to_owned());
        right.env_bundle = Some(bundle);

        assert_ne!(
            left.cache_key(&root).unwrap(),
            right.cache_key(&root).unwrap()
        );
        fs::remove_dir_all(root).unwrap();
    }

    fn test_action(root: &Path, input: &Path) -> Action {
        Action {
            id: "compile".to_owned(),
            mnemonic: "RustLib".to_owned(),
            program: root.join("rustc"),
            args: vec!["--crate-name".to_owned(), "demo".to_owned()],
            env_bundle: None,
            env: BTreeMap::from([("LANG".to_owned(), "C".to_owned())]),
            inputs: vec![input.to_path_buf()],
            outputs: vec![root.join("out.rlib")],
            workdir: root.to_path_buf(),
            key_material: BTreeMap::from([("profile".to_owned(), "debug".to_owned())]),
            stdout: None,
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("tong-{name}-{}-{nanos}", std::process::id()))
    }
}
