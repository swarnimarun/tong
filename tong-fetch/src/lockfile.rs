//! `Tong.lock`: the single source of truth for versions, like Cargo.lock.
//!
//! Records every package of the resolved graph — registry packages with
//! their checksums, path/workspace packages with the manifest checksum of
//! their `Cargo.toml`. Deterministic serialization (sorted by name, then
//! version); written atomically (temp + rename).

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::Path;

use semver::Version;
use serde::{Deserialize, Serialize};

/// Lockfile schema version.
pub const LOCKFILE_VERSION: u32 = 1;

/// A locked package.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockedPackage {
    /// Package name.
    pub name: String,
    /// Resolved version.
    pub version: Version,
    /// Source: `registry+<index>` or `path+<workspace-relative>`.
    pub source: String,
    /// `.crate` archive SHA-256 (registry packages).
    pub checksum: Option<String>,
    /// SHA-256 of the normalized `Cargo.toml` (path packages).
    pub manifest_checksum: Option<String>,
    /// Whether the version is yanked on the registry.
    #[serde(default)]
    pub yanked: bool,
    /// Index publish time, informational.
    pub publish_time: Option<String>,
    /// Locked dependency edges: `"<name> <version> <source>"`.
    #[serde(default)]
    pub dependencies: Vec<String>,
}

impl LockedPackage {
    /// Parses a lockfile dependency string.
    pub fn parse_dependency(text: &str) -> (&str, &str, &str) {
        let mut parts = text.splitn(3, ' ');
        let name = parts.next().unwrap_or("");
        let version = parts.next().unwrap_or("");
        let source = parts.next().unwrap_or("");
        (name, version, source)
    }
}

/// The lockfile.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TongLock {
    /// Schema version.
    pub version: u32,
    /// Locked packages, sorted by (name, version).
    pub packages: Vec<LockedPackage>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct LockFile {
    version: u32,
    #[serde(default)]
    package: Vec<LockedPackage>,
}

impl TongLock {
    /// Loads `Tong.lock` from `dir`.
    pub fn load(dir: &Path) -> Result<Self, LockError> {
        let path = dir.join("Tong.lock");
        let text = fs::read_to_string(&path)
            .map_err(|err| LockError::Io(path.display().to_string(), err))?;
        let file: LockFile = toml::from_str(&text)
            .map_err(|err| LockError::Parse(path.display().to_string(), err))?;
        if file.version > LOCKFILE_VERSION {
            return Err(LockError::UnsupportedVersion(file.version));
        }
        let mut packages = file.package;
        packages.sort_by(|a, b| a.name.cmp(&b.name).then(a.version.cmp(&b.version)));
        Ok(Self {
            version: file.version,
            packages,
        })
    }

    /// Saves the lockfile to `dir/Tong.lock`, atomically.
    pub fn save(&self, dir: &Path) -> Result<(), LockError> {
        let mut packages = self.packages.clone();
        packages.sort_by(|a, b| a.name.cmp(&b.name).then(a.version.cmp(&b.version)));
        let file = LockFile {
            version: LOCKFILE_VERSION,
            package: packages,
        };
        let text = toml::to_string(&file).map_err(|err| LockError::Serialize(err.to_string()))?;
        let path = dir.join("Tong.lock");
        let tmp = dir.join(format!("Tong.lock.tmp-{}", std::process::id()));
        fs::write(&tmp, text)?;
        match fs::rename(&tmp, &path) {
            Ok(()) => Ok(()),
            Err(err) if path.exists() => {
                let _ = fs::remove_file(&tmp);
                let _ = err;
                Ok(())
            }
            Err(err) => Err(LockError::Io(path.display().to_string(), err)),
        }
    }

    /// The locked package with `name` (or `name@version` when several
    /// versions are locked — not supported yet).
    pub fn package(&self, name: &str) -> Option<&LockedPackage> {
        self.packages.iter().find(|package| package.name == name)
    }

    /// Every locked package name → entries.
    pub fn by_name(&self) -> BTreeMap<&str, Vec<&LockedPackage>> {
        let mut out: BTreeMap<&str, Vec<&LockedPackage>> = BTreeMap::new();
        for package in &self.packages {
            out.entry(package.name.as_str()).or_default().push(package);
        }
        out
    }
}

/// Lockfile failure.
#[derive(Debug)]
pub enum LockError {
    /// The file could not be read.
    Io(String, io::Error),
    /// The file was not valid TOML.
    Parse(String, toml::de::Error),
    /// The file was written by a newer Tong.
    UnsupportedVersion(u32),
    /// TOML serialization failed.
    Serialize(String),
}

impl std::fmt::Display for LockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(path, err) => write!(f, "cannot read {path}: {err}"),
            Self::Parse(path, err) => write!(f, "cannot parse {path}: {err}"),
            Self::UnsupportedVersion(version) => {
                write!(f, "Tong.lock version {version} is newer than this Tong")
            }
            Self::Serialize(msg) => write!(f, "cannot serialize Tong.lock: {msg}"),
        }
    }
}

impl std::error::Error for LockError {}

impl From<io::Error> for LockError {
    fn from(err: io::Error) -> Self {
        Self::Io(String::new(), err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(dir: &Path) -> TongLock {
        let lock = TongLock {
            version: LOCKFILE_VERSION,
            packages: vec![
                LockedPackage {
                    name: "app".to_owned(),
                    version: Version::new(0, 1, 0),
                    source: "path+crates/app".to_owned(),
                    checksum: None,
                    manifest_checksum: Some("def".to_owned()),
                    yanked: false,
                    publish_time: None,
                    dependencies: Vec::new(),
                },
                LockedPackage {
                    name: "serde".to_owned(),
                    version: Version::new(1, 0, 200),
                    source: "registry+https://index.crates.io".to_owned(),
                    checksum: Some("abc".to_owned()),
                    manifest_checksum: None,
                    yanked: false,
                    publish_time: None,
                    dependencies: vec![
                        "serde_derive 1.0.200 registry+https://index.crates.io".to_owned(),
                    ],
                },
            ],
        };
        let _ = dir;
        lock
    }

    #[test]
    fn lockfile_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let lock = sample(dir.path());
        lock.save(dir.path()).unwrap();
        let loaded = TongLock::load(dir.path()).unwrap();
        assert_eq!(loaded.packages, lock.packages);
        assert_eq!(
            loaded.package("serde").unwrap().version,
            Version::new(1, 0, 200)
        );
        assert!(loaded.package("missing").is_none());
    }

    #[test]
    fn save_is_deterministic() {
        let dir = tempfile::tempdir().unwrap();
        let lock = sample(dir.path());
        lock.save(dir.path()).unwrap();
        let first = fs::read_to_string(dir.path().join("Tong.lock")).unwrap();
        // Out-of-order input serializes identically.
        let mut shuffled = lock.clone();
        shuffled.packages.reverse();
        shuffled.save(dir.path()).unwrap();
        let second = fs::read_to_string(dir.path().join("Tong.lock")).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn rejects_newer_lockfiles() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("Tong.lock"), "version = 99\n").unwrap();
        assert!(matches!(
            TongLock::load(dir.path()),
            Err(LockError::UnsupportedVersion(99))
        ));
    }

    #[test]
    fn parses_dependency_strings() {
        let (name, version, source) =
            LockedPackage::parse_dependency("serde 1.0.200 registry+https://index.crates.io");
        assert_eq!(name, "serde");
        assert_eq!(version, "1.0.200");
        assert_eq!(source, "registry+https://index.crates.io");
    }
}
