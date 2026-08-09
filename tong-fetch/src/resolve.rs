//! Cargo-style version resolution (pure, deterministic).
//!
//! Greedy highest-first DFS with backtracking: candidate versions come
//! from the sparse index (non-yanked, unless already in the lockfile, where
//! the locked version is preferred for stability). On a unification
//! conflict (two requirements with no common version) the resolver
//! backtracks to the most recent package choice with a remaining
//! candidate, restores the state before that choice, and re-checks every
//! edge that touches the package (consumed edges are replayed from the
//! history). Optional deps of registry packages are locked unconditionally
//! (Cargo's lockfile completeness — build-time feature resolution decides
//! what is compiled); workspace-member edges arrive feature-filtered from
//! the caller (Phase B).

use std::collections::BTreeMap;

use semver::{Version, VersionReq};

use crate::lockfile::TongLock;
use crate::registry::FetchError;
use crate::sparse_index::{IndexDepKind, IndexVersion};

/// A version requirement edge in the resolution graph.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedDep {
    /// Package name.
    pub name: String,
    /// Version requirement.
    pub req: VersionReq,
    /// Features requested on the dependency.
    pub features: Vec<String>,
    /// Optional dependency.
    pub optional: bool,
    /// Whether the dependency's default feature is enabled.
    pub default_features: bool,
    /// Dependency kind.
    pub kind: DepKind,
    /// Registry the dependency comes from (`None` = the default).
    pub registry: Option<String>,
}

/// Dependency kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DepKind {
    /// `[dependencies]`.
    Normal,
    /// `[dev-dependencies]`.
    Dev,
    /// `[build-dependencies]`.
    Build,
}

impl From<IndexDepKind> for DepKind {
    fn from(kind: IndexDepKind) -> Self {
        match kind {
            IndexDepKind::Normal => Self::Normal,
            IndexDepKind::Dev => Self::Dev,
            IndexDepKind::Build => Self::Build,
        }
    }
}

/// A resolved registry package.
#[derive(Clone, Debug)]
pub struct ResolvedPackage {
    /// Package name.
    pub name: String,
    /// Resolved version.
    pub version: Version,
    /// `.crate` archive SHA-256 (from the index).
    pub checksum: String,
    /// Dependency edges (normal + build; optional included for lockfile
    /// completeness).
    pub dependencies: Vec<ResolvedDep>,
    /// Declared features (from the index).
    pub features: BTreeMap<String, Vec<String>>,
    /// Whether the chosen version is yanked (allowed when locked).
    pub yanked: bool,
}

/// A source of index data (implemented by [`crate::IndexClient`]; tests use
/// a fixture double).
pub trait CrateSource {
    /// Every index version entry for `name`.
    fn versions(&self, name: &str) -> Result<Vec<IndexVersion>, FetchError>;
}

/// Resolution failure.
#[derive(Debug)]
pub enum ResolveError {
    /// No version of a package satisfies its requirements.
    NoMatchingVersion {
        /// Package name.
        package: String,
        /// The requirements that could not be satisfied.
        reqs: Vec<String>,
    },
    /// The requirement set is unsatisfiable after backtracking.
    Unresolvable {
        /// Human-readable description of the conflicting requirements.
        chain: String,
    },
    /// Index fetch failure.
    Fetch(FetchError),
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoMatchingVersion { package, reqs } => {
                write!(
                    f,
                    "no matching version of `{package}` found for requirements {}",
                    reqs.join(", ")
                )
            }
            Self::Unresolvable { chain } => {
                write!(f, "unable to resolve dependencies: {chain}")
            }
            Self::Fetch(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for ResolveError {}

impl From<FetchError> for ResolveError {
    fn from(err: FetchError) -> Self {
        Self::Fetch(err)
    }
}

/// A choice frame for backtracking.
struct Frame {
    /// The package being chosen.
    package: String,
    /// Candidate versions still to try, ascending (pop() = next highest).
    remaining: Vec<Version>,
}

/// A worklist entry: a requirement edge plus the package+version that
/// generated it (root edges have no generator).
#[derive(Clone, Debug)]
struct Pending {
    edge: ResolvedDep,
    generated_by: Option<(String, Version)>,
}

/// State snapshot taken before a package choice, restored on backtrack.
struct Snapshot {
    chosen: BTreeMap<String, ResolvedPackage>,
    worklist: Vec<Pending>,
    history: Vec<Pending>,
}

/// Resolves versions for `roots` (feature-filtered workspace-member edges),
/// preferring `locked` versions and allowing their yanked entries.
pub fn resolve(
    crates: &dyn CrateSource,
    roots: &[ResolvedDep],
    locked: &TongLock,
) -> Result<Vec<ResolvedPackage>, ResolveError> {
    let mut chosen: BTreeMap<String, ResolvedPackage> = BTreeMap::new();
    let mut worklist: Vec<Pending> = roots
        .iter()
        .cloned()
        .map(|edge| Pending {
            edge,
            generated_by: None,
        })
        .collect();
    sort_worklist(&mut worklist);
    let mut history: Vec<Pending> = Vec::new();
    let mut frames: Vec<Frame> = Vec::new();
    let mut snapshots: Vec<Snapshot> = Vec::new();

    while let Some(pending) = worklist.pop() {
        let edge = &pending.edge;
        history.push(pending.clone());
        match chosen.get(&edge.name) {
            Some(pkg) if edge.req.matches(&pkg.version) => continue,
            Some(_) => {
                // Unification conflict: backtrack to a re-openable choice.
                let Some(rechosen) = backtrack(
                    crates,
                    &mut chosen,
                    &mut worklist,
                    &mut history,
                    &mut frames,
                    &mut snapshots,
                )?
                else {
                    return Err(ResolveError::Unresolvable {
                        chain: format!(
                            "{} {} conflicts with the already-resolved version of `{}`",
                            edge.name, edge.req, edge.name
                        ),
                    });
                };
                // The edge is still live unless its generator was
                // re-chosen (whose new dep set supersedes the old one).
                let stale = pending
                    .generated_by
                    .as_ref()
                    .is_some_and(|(package, _)| package == &rechosen);
                if !stale {
                    worklist.push(pending);
                    sort_worklist(&mut worklist);
                }
            }
            None => {
                let name = edge.name.clone();
                let mut candidates = candidates(crates, locked, &edge.name, &edge.req)?;
                if candidates.is_empty() {
                    return Err(ResolveError::NoMatchingVersion {
                        package: edge.name.clone(),
                        reqs: vec![edge.req.to_string()],
                    });
                }
                snapshots.push(Snapshot {
                    chosen: chosen.clone(),
                    worklist: worklist.clone(),
                    history: history.clone(),
                });
                let version = candidates.remove(0);
                frames.push(Frame {
                    package: name.clone(),
                    remaining: candidates.into_iter().rev().collect(),
                });
                let package = choose(crates, &name, &version)?;
                let mut deps: Vec<Pending> = package
                    .dependencies
                    .iter()
                    .filter(|dep| dep.kind != DepKind::Dev)
                    .cloned()
                    .map(|edge| Pending {
                        edge,
                        generated_by: Some((name.clone(), version.clone())),
                    })
                    .collect();
                sort_worklist(&mut deps);
                worklist.append(&mut deps);
                worklist.push(pending);
                sort_worklist(&mut worklist);
                chosen.insert(name, package);
            }
        }
    }

    let mut out: Vec<ResolvedPackage> = chosen.into_values().collect();
    out.sort_by(|a, b| a.name.cmp(&b.name).then(a.version.cmp(&b.version)));
    Ok(out)
}

/// Builds a [`ResolvedPackage`] for the given version from the index.
fn choose(
    crates: &dyn CrateSource,
    name: &str,
    version: &Version,
) -> Result<ResolvedPackage, ResolveError> {
    let entry = crates
        .versions(name)?
        .into_iter()
        .find(|entry| &entry.vers == version)
        .ok_or_else(|| {
            ResolveError::Fetch(FetchError::BadConfig(format!(
                "index entry for {name} {version} vanished during resolution"
            )))
        })?;
    Ok(package_from_index(name, &entry))
}

/// Builds a [`ResolvedPackage`] from an index entry.
fn package_from_index(name: &str, entry: &IndexVersion) -> ResolvedPackage {
    let dependencies = entry
        .deps
        .iter()
        .map(|dep| ResolvedDep {
            name: dep.package.clone().unwrap_or_else(|| dep.name.clone()),
            req: dep.req.clone(),
            features: dep.features.clone(),
            optional: dep.optional,
            default_features: dep.default_features,
            kind: dep.kind.into(),
            registry: None,
        })
        .collect();
    ResolvedPackage {
        name: name.to_owned(),
        version: entry.vers.clone(),
        checksum: entry.cksum.clone(),
        dependencies,
        features: entry.features.clone(),
        yanked: entry.yanked,
    }
}

/// Candidate versions for `(name, req)`: index versions matching the
/// requirement, not yanked unless already in the lockfile; the locked
/// version (when it matches) is preferred for lockfile stability.
fn candidates(
    crates: &dyn CrateSource,
    locked: &TongLock,
    name: &str,
    req: &VersionReq,
) -> Result<Vec<Version>, ResolveError> {
    let locked_version = locked.package(name).map(|package| package.version.clone());
    let mut versions: Vec<Version> = crates
        .versions(name)?
        .into_iter()
        .filter(|entry| req.matches(&entry.vers))
        .filter(|entry| !entry.yanked || locked_version.as_ref() == Some(&entry.vers))
        .map(|entry| entry.vers)
        .collect();
    versions.sort();
    versions.reverse();
    if let Some(locked) = &locked_version
        && req.matches(locked)
        && let Some(index) = versions.iter().position(|v| v == locked)
    {
        versions.remove(index);
        versions.insert(0, locked.clone());
    }
    Ok(versions)
}

/// Backtracks: pops choice frames until one has a remaining candidate,
/// restores its snapshot, and re-chooses with the next candidate, replaying
/// every historical edge that touches the re-chosen package. Returns the
/// re-chosen package, or `None` when no frame can be re-opened.
fn backtrack(
    crates: &dyn CrateSource,
    chosen: &mut BTreeMap<String, ResolvedPackage>,
    worklist: &mut Vec<Pending>,
    history: &mut Vec<Pending>,
    frames: &mut Vec<Frame>,
    snapshots: &mut Vec<Snapshot>,
) -> Result<Option<String>, ResolveError> {
    while let (Some(mut frame), Some(snapshot)) = (frames.pop(), snapshots.pop()) {
        if frame.remaining.is_empty() {
            continue;
        }
        // Restore the state before this package was chosen.
        *chosen = snapshot.chosen;
        *worklist = snapshot.worklist;
        *history = snapshot.history;
        let version = frame.remaining.pop().expect("non-empty");
        let package = choose(crates, &frame.package, &version)?;
        let mut deps: Vec<Pending> = package
            .dependencies
            .iter()
            .filter(|dep| dep.kind != DepKind::Dev)
            .cloned()
            .map(|edge| Pending {
                edge,
                generated_by: Some((frame.package.clone(), version.clone())),
            })
            .collect();
        sort_worklist(&mut deps);
        worklist.append(&mut deps);
        // Replay every edge that touches this package so previously
        // satisfied requirements are re-checked against the new version.
        let touching: Vec<Pending> = history
            .iter()
            .filter(|pending| pending.edge.name == frame.package)
            .cloned()
            .collect();
        worklist.extend(touching);
        sort_worklist(worklist);
        chosen.insert(frame.package.clone(), package);
        // A fresh frame for future backtracking.
        frames.push(Frame {
            package: frame.package.clone(),
            remaining: frame.remaining,
        });
        snapshots.push(Snapshot {
            chosen: chosen.clone(),
            worklist: worklist.clone(),
            history: history.clone(),
        });
        return Ok(Some(frame.package));
    }
    Ok(None)
}

/// Sorts worklist entries deterministically (by name, then kind).
fn sort_worklist(worklist: &mut [Pending]) {
    worklist.sort_by(|a, b| {
        a.edge
            .name
            .cmp(&b.edge.name)
            .then_with(|| format!("{:?}", a.edge.kind).cmp(&format!("{:?}", b.edge.kind)))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// A fixture index: name → index entries.
    struct Fixture(BTreeMap<String, Vec<IndexVersion>>);

    impl CrateSource for Fixture {
        fn versions(&self, name: &str) -> Result<Vec<IndexVersion>, FetchError> {
            Ok(self.0.get(name).cloned().unwrap_or_default())
        }
    }

    fn entry(name: &str, version: &str, deps: &[(&str, &str, bool)]) -> IndexVersion {
        IndexVersion {
            name: name.to_owned(),
            vers: Version::parse(version).unwrap(),
            deps: deps
                .iter()
                .map(|(dep, req, optional)| crate::sparse_index::IndexDep {
                    name: (*dep).to_owned(),
                    req: VersionReq::parse(req).unwrap(),
                    features: Vec::new(),
                    optional: *optional,
                    default_features: true,
                    target: None,
                    kind: IndexDepKind::Normal,
                    package: None,
                })
                .collect(),
            cksum: format!("cksum-{name}-{version}"),
            features: BTreeMap::new(),
            features2: None,
            yanked: false,
            rust_version: None,
            v: 1,
        }
    }

    fn edge(name: &str, req: &str) -> ResolvedDep {
        ResolvedDep {
            name: name.to_owned(),
            req: VersionReq::parse(req).unwrap(),
            features: Vec::new(),
            optional: false,
            default_features: true,
            kind: DepKind::Normal,
            registry: None,
        }
    }

    fn fixture() -> Fixture {
        // a 1.0 requires b ^1; a 2.0 requires b ^2.
        let mut index = BTreeMap::new();
        index.insert(
            "a".to_owned(),
            vec![
                entry("a", "2.0.0", &[("b", "^2", false)]),
                entry("a", "1.0.0", &[("b", "^1", false)]),
            ],
        );
        index.insert(
            "b".to_owned(),
            vec![entry("b", "1.0.0", &[]), entry("b", "2.0.0", &[])],
        );
        Fixture(index)
    }

    #[test]
    fn picks_highest_matching_non_yanked() {
        let index = fixture();
        let locked = TongLock::default();
        let packages = resolve(&index, &[edge("a", "*")], &locked).unwrap();
        assert_eq!(packages.len(), 2);
        let a = packages.iter().find(|p| p.name == "a").unwrap();
        let b = packages.iter().find(|p| p.name == "b").unwrap();
        assert_eq!(a.version, Version::new(2, 0, 0));
        assert_eq!(b.version, Version::new(2, 0, 0));
    }

    #[test]
    fn prefers_locked_version() {
        let index = fixture();
        let mut locked = TongLock::default();
        locked.packages.push(crate::lockfile::LockedPackage {
            name: "a".to_owned(),
            version: Version::new(1, 0, 0),
            source: "registry+https://index.crates.io".to_owned(),
            checksum: Some("x".to_owned()),
            manifest_checksum: None,
            yanked: false,
            publish_time: None,
            dependencies: Vec::new(),
        });
        let packages = resolve(&index, &[edge("a", "*")], &locked).unwrap();
        let a = packages.iter().find(|p| p.name == "a").unwrap();
        assert_eq!(a.version, Version::new(1, 0, 0));
    }

    #[test]
    fn backtracks_on_conflict() {
        // Root requires a * and b ^1: greedy picks a 2.0, which needs b ^2;
        // backtracking must land on a 1.0.
        let index = fixture();
        let locked = TongLock::default();
        let packages = resolve(&index, &[edge("a", "*"), edge("b", "^1")], &locked).unwrap();
        let a = packages.iter().find(|p| p.name == "a").unwrap();
        let b = packages.iter().find(|p| p.name == "b").unwrap();
        assert_eq!(a.version, Version::new(1, 0, 0));
        assert_eq!(b.version, Version::new(1, 0, 0));
    }

    #[test]
    fn unresolvable_requirements_error() {
        // a 1.0 requires b ^2, root wants b ^1: no solution.
        let mut index = BTreeMap::new();
        index.insert(
            "a".to_owned(),
            vec![entry("a", "1.0.0", &[("b", "^2", false)])],
        );
        index.insert("b".to_owned(), vec![entry("b", "1.0.0", &[])]);
        let locked = TongLock::default();
        let err =
            resolve(&Fixture(index), &[edge("a", "*"), edge("b", "^1")], &locked).unwrap_err();
        assert!(matches!(err, ResolveError::Unresolvable { .. }), "{err}");
    }

    #[test]
    fn no_matching_version_error() {
        let mut index = BTreeMap::new();
        index.insert("a".to_owned(), vec![entry("a", "1.0.0", &[])]);
        let locked = TongLock::default();
        let err = resolve(&Fixture(index), &[edge("a", "^2")], &locked).unwrap_err();
        assert!(
            matches!(err, ResolveError::NoMatchingVersion { .. }),
            "{err}"
        );
    }

    #[test]
    fn locked_yanked_versions_are_allowed() {
        let mut index = BTreeMap::new();
        let mut yanked = entry("a", "1.0.0", &[]);
        yanked.yanked = true;
        index.insert("a".to_owned(), vec![yanked, entry("a", "2.0.0", &[])]);
        let mut locked = TongLock::default();
        locked.packages.push(crate::lockfile::LockedPackage {
            name: "a".to_owned(),
            version: Version::new(1, 0, 0),
            source: "registry+https://index.crates.io".to_owned(),
            checksum: Some("x".to_owned()),
            manifest_checksum: None,
            yanked: true,
            publish_time: None,
            dependencies: Vec::new(),
        });
        let packages = resolve(&Fixture(index), &[edge("a", "*")], &locked).unwrap();
        let a = packages.iter().find(|p| p.name == "a").unwrap();
        assert_eq!(a.version, Version::new(1, 0, 0));
        assert!(a.yanked);
    }
}
