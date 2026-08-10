//! `Tong.lock`: the single source of truth for versions, like Cargo.lock.
//!
//! Records every package of the resolved graph — registry packages with
//! their checksums, path/workspace packages with the manifest checksum of
//! their `Cargo.toml`. Deterministic serialization (sorted by name, then
//! version, then source); written atomically (temp + rename).
//!
//! Version 2 keys dependency edges by the full `(name, version, source)`
//! tuple — a name may lock several versions or sources. Version 1 locks
//! are loaded through an in-memory migration that succeeds only when every
//! referenced name is unambiguous; ambiguous v1 locks fail with a
//! regeneration diagnostic.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::Path;

use semver::Version;
use serde::{Deserialize, Serialize};

/// Lockfile schema version. Version 2: identity is `(name, version,
/// source)`; packages and dependency strings sort by that tuple.
pub const LOCKFILE_VERSION: u32 = 2;

/// A locked package.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockedPackage {
    /// Package name.
    pub name: String,
    /// Resolved version.
    pub version: Version,
    /// Source: `registry+<index>`, `path+<workspace-relative>`, or
    /// `git+<url>#<commit>`.
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

    /// The identity tuple `(name, version, source)` as a lock string.
    pub fn identity(&self) -> String {
        format!("{} {} {}", self.name, self.version, self.source)
    }
}

/// The lockfile.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TongLock {
    /// Schema version.
    pub version: u32,
    /// Locked packages, sorted by (name, version, source).
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
    ///
    /// Version 1 files are migrated in memory: they load only when every
    /// name referenced by a dependency string (and every locked name) has
    /// exactly one candidate — otherwise the lock cannot key edges exactly
    /// and the caller must regenerate it.
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
        packages.sort_by(|a, b| {
            a.name
                .cmp(&b.name)
                .then(a.version.cmp(&b.version))
                .then(a.source.cmp(&b.source))
        });
        if file.version < 2 {
            let by_name = count_by_name(&packages);
            for package in &packages {
                if by_name.get(package.name.as_str()).copied().unwrap_or(0) > 1 {
                    return Err(LockError::Ambiguous(package.name.clone()));
                }
                for dep in &package.dependencies {
                    let (name, _, _) = LockedPackage::parse_dependency(dep);
                    if by_name.get(name).copied().unwrap_or(0) > 1 {
                        return Err(LockError::Ambiguous(name.to_owned()));
                    }
                }
            }
        }
        Ok(Self {
            version: LOCKFILE_VERSION,
            packages,
        })
    }

    /// Saves the lockfile to `dir/Tong.lock`, atomically.
    pub fn save(&self, dir: &Path) -> Result<(), LockError> {
        let mut packages = self.packages.clone();
        packages.sort_by(|a, b| {
            a.name
                .cmp(&b.name)
                .then(a.version.cmp(&b.version))
                .then(a.source.cmp(&b.source))
        });
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

    /// Every locked package with `name`.
    pub fn candidates(&self, name: &str) -> impl Iterator<Item = &LockedPackage> {
        self.packages
            .iter()
            .filter(move |package| package.name == name)
    }

    /// The locked package with the exact identity tuple `(name, version,
    /// source)`.
    pub fn exact(&self, name: &str, version: &Version, source: &str) -> Option<&LockedPackage> {
        self.packages.iter().find(|package| {
            package.name == name && package.version == *version && package.source == source
        })
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

fn count_by_name(packages: &[LockedPackage]) -> BTreeMap<&str, usize> {
    let mut out: BTreeMap<&str, usize> = BTreeMap::new();
    for package in packages {
        *out.entry(package.name.as_str()).or_default() += 1;
    }
    out
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
    /// A version-1 lock cannot key edges exactly because the name has
    /// several candidates.
    Ambiguous(String),
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
            Self::Ambiguous(name) => write!(
                f,
                "Tong.lock v1 is ambiguous for package {name}; run `tong lock`"
            ),
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
            loaded.candidates("serde").next().unwrap().version,
            Version::new(1, 0, 200)
        );
        assert!(loaded.candidates("missing").next().is_none());
        // Saved locks are version 2.
        assert_eq!(loaded.version, LOCKFILE_VERSION);
        assert_eq!(loaded.version, 2);
        // Exact tuple lookup.
        assert!(
            loaded
                .exact(
                    "serde",
                    &Version::new(1, 0, 200),
                    "registry+https://index.crates.io"
                )
                .is_some()
        );
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
        assert!(first.starts_with("version = 2"), "{first}");
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

    /// A version-1 lock with unique names loads (in-memory migration).
    #[test]
    fn v1_lock_migrates_when_unambiguous() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("Tong.lock"),
            r#"
version = 1

[[package]]
name = "app"
version = "0.1.0"
source = "path+crates/app"

[[package]]
name = "serde"
version = "1.0.200"
source = "registry+https://index.crates.io"
checksum = "abc"
dependencies = ["serde_derive 1.0.200 registry+https://index.crates.io"]

[[package]]
name = "serde_derive"
version = "1.0.200"
source = "registry+https://index.crates.io"
checksum = "def"
"#,
        )
        .unwrap();
        let lock = TongLock::load(dir.path()).unwrap();
        assert_eq!(lock.version, 2);
        assert_eq!(lock.packages.len(), 3);
        assert_eq!(
            lock.candidates("serde").next().unwrap().version,
            Version::new(1, 0, 200)
        );
    }

    /// A version-1 lock referencing an ambiguous name fails with the
    /// regeneration diagnostic.
    #[test]
    fn lock_v1_ambiguity_is_a_targeted_error() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("Tong.lock"),
            r#"
version = 1

[[package]]
name = "app"
version = "0.1.0"
source = "path+crates/app"
dependencies = ["alpha 1.0.0 registry+fixture"]

[[package]]
name = "alpha"
version = "1.0.0"
source = "registry+fixture"

[[package]]
name = "alpha"
version = "2.0.0"
source = "registry+fixture"
"#,
        )
        .unwrap();
        let err = TongLock::load(dir.path()).unwrap_err();
        assert!(
            err.to_string()
                .contains("Tong.lock v1 is ambiguous for package alpha"),
            "{err}"
        );
    }
}
