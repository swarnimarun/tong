use crate::hash::StableHasher;
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvBundle {
    pub id: String,
    pub kind: EnvBundleKind,
    pub vars: BTreeMap<String, String>,
    pub key_material: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvBundleKind {
    HostToolchain,
    NixClosure,
    WindowsToolchain,
}

impl EnvBundle {
    pub fn new(id: impl Into<String>, kind: EnvBundleKind) -> Self {
        Self {
            id: id.into(),
            kind,
            vars: BTreeMap::new(),
            key_material: BTreeMap::new(),
        }
    }

    pub fn host_rust_toolchain() -> Option<Self> {
        if cfg!(windows) {
            let mut bundle = Self::new(
                "host-rust-toolchain-windows",
                EnvBundleKind::WindowsToolchain,
            );
            for key in ["PATH", "LIB", "LIBPATH", "INCLUDE"] {
                if let Some(value) = std::env::var_os(key) {
                    bundle
                        .vars
                        .insert(key.to_owned(), value.to_string_lossy().into_owned());
                }
            }
            bundle
                .key_material
                .insert("platform".to_owned(), "windows".to_owned());
            return Some(bundle);
        }

        None
    }

    pub fn fingerprint(&self) -> String {
        let mut hasher = StableHasher::new();
        hasher.update_str("tong-env-bundle-v1");
        hasher.update_str(&self.id);
        hasher.update_str(self.kind.as_str());
        for (key, value) in &self.vars {
            hasher.update_str("var");
            hasher.update_str(key);
            hasher.update_str(value);
        }
        for (key, value) in &self.key_material {
            hasher.update_str("material");
            hasher.update_str(key);
            hasher.update_str(value);
        }
        hasher.finish_hex()
    }
}

impl EnvBundleKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::HostToolchain => "host-toolchain",
            Self::NixClosure => "nix-closure",
            Self::WindowsToolchain => "windows-toolchain",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_fingerprint_changes_with_vars() {
        let mut first = EnvBundle::new("demo", EnvBundleKind::HostToolchain);
        first.vars.insert("PATH".to_owned(), "one".to_owned());

        let mut second = EnvBundle::new("demo", EnvBundleKind::HostToolchain);
        second.vars.insert("PATH".to_owned(), "two".to_owned());

        assert_ne!(first.fingerprint(), second.fingerprint());
    }

    #[test]
    fn bundle_fingerprint_changes_with_kind() {
        let first = EnvBundle::new("demo", EnvBundleKind::HostToolchain);
        let second = EnvBundle::new("demo", EnvBundleKind::NixClosure);

        assert_ne!(first.fingerprint(), second.fingerprint());
    }
}
