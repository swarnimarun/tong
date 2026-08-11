//! Cargo-compatible version resolution (pure, deterministic).
//!
//! Ported from cargo's resolver (`cargo/src/resolver/mod.rs`,
//! `context.rs`, `conflict_cache.rs`, `types.rs` — MIT OR Apache-2.0,
//! Copyright The Cargo Developers; port adapted for tong's model):
//! greedy highest-first DFS with backtracking and a global conflict cache.
//!
//! The two properties that make this terminate on real graphs (and that
//! tong's earlier one-version-per-name backtracker lacked):
//!
//! 1. **Semver-compatible activation keys.** A package is activated once
//!    per semver-compatibility group (`major`, or `0.minor`, or `0.0.patch`).
//!    Semver-incompatible versions — `syn 2.x` and `syn 3.x` — are
//!    different activations and coexist in one lockfile, exactly like
//!    cargo's. Only semver-compatible versions conflict.
//! 2. **Backtrack frames with full context snapshots + conflict cache.**
//!    Every candidate attempt is a cheap clone; on exhaustion the resolver
//!    records the conflict set ("this dependency cannot resolve while these
//!    packages are active") in a global trie and backjumps to the newest
//!    frame that can change the outcome, skipping provably-dead frames.
//!
//! Workspace/path packages are part of the graph (roots): they activate at
//! their exact versions and every dependency edge records the resolved
//! version, so lockfiles stay correct when a name has multiple versions.
//! Registry packages lock all non-dev dependencies (feature resolution
//! happens separately, `tong-rust::resolve_features`).

use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use semver::{Version, VersionReq};
use tracing::{debug, info, warn};

use crate::lockfile::TongLock;
use crate::sparse_index::{IndexDepKind, IndexVersion};

/// A version requirement edge in the resolution graph.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedDep {
    /// The declared dependency name (the alias for renamed deps: deadpool
    /// references `tokio_1 = { package = "tokio" }` by `tokio_1`).
    pub name: String,
    /// The real crate name when the dependency is renamed (`Some`); the
    /// index lookup and lock edges use it.
    pub package: Option<String>,
    /// Version requirement (`None` for local/path edges).
    pub req: Option<VersionReq>,
    /// Optional (feature-activated) dependency.
    pub optional: bool,
    /// Dev-dependency edge.
    pub dev: bool,
    /// Features requested on the target by this edge.
    pub features: Vec<String>,
    /// Whether the target's default features are requested.
    pub default_features: bool,
}

/// A local (workspace/path) package: activates at its exact version without
/// an index query, and its dev-dependencies are locked (cargo semantics for
/// workspace members).
#[derive(Clone, Debug)]
pub struct LocalPackage {
    pub name: String,
    pub version: Version,
    /// Lockfile source string (`path+<rel>`) of the local package. Carried
    /// through resolution so a local `foo` and a registry `foo` never
    /// alias in lock assembly.
    pub source: Option<String>,
    /// Dependencies: `None` req = local/path edge, `Some` = registry edge.
    pub deps: Vec<ResolvedDep>,
}

/// A resolved package in the lockfile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedPackage {
    pub name: String,
    pub version: Version,
    /// Lockfile source string: `Some("path+…")` for workspace/path
    /// packages, `None` for registry packages (the caller knows the index
    /// URL).
    pub source: Option<String>,
    /// Registry checksum (`None` for local packages).
    pub checksum: Option<String>,
    pub yanked: bool,
    /// Whether this is a workspace/path package (no registry source).
    pub local: bool,
    /// Resolved dependency edges: exact locked versions (deduplicated),
    /// with the target's source string when it is a local package.
    pub dependencies: Vec<(String, Version, Option<String>)>,
}

/// A source of index data (implemented by [`crate::IndexClient`]; tests use
/// a fixture double).
pub trait CrateSource {
    fn versions(&self, name: &str) -> Result<Vec<IndexVersion>, crate::FetchError>;
}

/// Resolution failure.
#[derive(Debug)]
pub enum ResolveError {
    /// No version of a package matches a requirement.
    NoMatchingVersion { package: String, reqs: Vec<String> },
    /// Requirements on a package are unsatisfiable together.
    Unresolvable { chain: String },
    /// Index failure.
    Fetch(crate::FetchError),
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
            Self::Unresolvable { chain } => write!(f, "{chain}"),
            Self::Fetch(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for ResolveError {}

impl From<crate::FetchError> for ResolveError {
    fn from(err: crate::FetchError) -> Self {
        Self::Fetch(err)
    }
}

// ---------------------------------------------------------------------------
// Ported types (cargo src/resolver/{types,conflict_cache,context}.rs)
// ---------------------------------------------------------------------------

/// A package's activation key: name + semver-compatibility group + source
/// (a local `foo` and a registry `foo` never conflict).
type ActivationKey = (String, SemverCompat, Option<String>);

fn activation_key(id: &PackageId) -> ActivationKey {
    (
        id.name.clone(),
        SemverCompat::from(&id.version),
        id.source.clone(),
    )
}

/// A package identity in the graph: name + exact version + source.
///
/// The source (`path+…` for local packages, `None` for registry) is part
/// of the identity so a local `foo` and a registry `foo` never alias in
/// activations, conflicts, or edges.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct PackageId {
    name: String,
    version: Version,
    source: Option<String>,
}

impl PackageId {
    fn new(name: &str, version: &Version, source: Option<String>) -> Self {
        Self {
            name: name.to_owned(),
            version: version.clone(),
            source,
        }
    }
}

/// Cargo's `SemverCompatibility`: the group within which only one version
/// may be activated. `1.0.2` and `1.2.0` share `Major(1)`; `0.1.x` and
/// `0.2.x` differ; `0.0.x` is per-patch.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum SemverCompat {
    Major(u64),
    Minor(u64),
    Patch(u64),
}

impl From<&Version> for SemverCompat {
    fn from(ver: &Version) -> Self {
        if ver.major > 0 {
            SemverCompat::Major(ver.major)
        } else if ver.minor > 0 {
            SemverCompat::Minor(ver.minor)
        } else {
            SemverCompat::Patch(ver.patch)
        }
    }
}

/// The summary of one package version: the unit of activation.
#[derive(Clone, Debug)]
struct Summary {
    id: PackageId,
    /// All dependency edges (registry and local).
    deps: Rc<Vec<ResolvedDep>>,
    /// The package's feature table (feature → references), used to decide
    /// which optional deps are enabled (cargo `build_requirements`).
    features: BTreeMap<String, Vec<String>>,
    checksum: Option<String>,
    yanked: bool,
    /// Workspace/path package: fixed version, dev-deps locked.
    local: bool,
    /// Lockfile source string (`path+<rel>`) for local packages.
    source: Option<String>,
}

/// Why a candidate was rejected.
#[derive(Clone, Debug, PartialEq, Eq)]
enum ConflictReason {
    /// A semver-compatible version of the same package is already active.
    Semver,
}

/// Activation outcome: a recoverable conflict, or a fatal error (index
/// failure, missing local edge) that aborts resolution.
enum ActivateError {
    Fatal(ResolveError),
    Conflict(PackageId, ConflictReason),
}

/// Package → reason, for one failed activation attempt.
type ConflictMap = BTreeMap<PackageId, ConflictReason>;

/// A cheap cloneable iterator over an `Rc<Vec<T>>` (cargo `RcVecIter`).
struct RcVecIter<T: Clone> {
    vec: Rc<Vec<T>>,
    idx: usize,
}

impl<T: Clone> RcVecIter<T> {
    fn new(vec: Rc<Vec<T>>) -> Self {
        Self { vec, idx: 0 }
    }
    /// A non-advancing view of the not-yet-consumed items.
    fn remaining(&self) -> impl Iterator<Item = &T> + '_ {
        self.vec.get(self.idx..).into_iter().flatten()
    }
    fn peek(&self) -> Option<&T> {
        self.vec.get(self.idx)
    }
}

impl<T: Clone> Iterator for RcVecIter<T> {
    type Item = T;
    fn next(&mut self) -> Option<T> {
        let item = self.vec.get(self.idx)?.clone();
        self.idx += 1;
        Some(item)
    }
}

impl<T: Clone> Clone for RcVecIter<T> {
    fn clone(&self) -> Self {
        Self {
            vec: Rc::clone(&self.vec),
            idx: self.idx,
        }
    }
}

/// A dependency edge plus its candidate summaries.
/// One dependency of a resolved package: the edge, its candidate
/// versions, and whether the target is expanded (recursed into).
/// Inactive optional deps are version-locked and recorded as edges but
/// not expanded — cargo's Cargo.lock lists every dependency of a
/// resolved package without pulling in the targets' own optional
/// closures.
#[derive(Clone)]
struct DepInfo {
    dep: ResolvedDep,
    candidates: Rc<Vec<Summary>>,
    expand: bool,
}

/// The pending deps of one activated package (cargo `DepsFrame`).
#[derive(Clone)]
struct DepsFrame {
    parent: Summary,
    /// Sorted with the fewest candidates first (most constrained).
    remaining_siblings: RcVecIter<DepInfo>,
}

impl DepsFrame {
    /// The least number of candidates of any remaining sibling.
    fn min_candidates(&self) -> usize {
        self.remaining_siblings
            .peek()
            .map(|info| info.candidates.len())
            .unwrap_or(0)
    }
}

impl PartialEq for DepsFrame {
    fn eq(&self, other: &Self) -> bool {
        self.min_candidates() == other.min_candidates()
    }
}
impl Eq for DepsFrame {}
impl PartialOrd for DepsFrame {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for DepsFrame {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.min_candidates()
            .cmp(&other.min_candidates())
            .then_with(|| self.parent.id.name.cmp(&other.parent.id.name))
    }
}

/// The set of pending dependency frames, most constrained first (cargo
/// `RemainingDeps`; a monotonic counter keeps equal frames distinct).
#[derive(Clone)]
struct RemainingDeps {
    time: u64,
    data: BTreeSet<(DepsFrame, u64)>,
}

impl RemainingDeps {
    fn new() -> Self {
        Self {
            time: 0,
            data: BTreeSet::new(),
        }
    }
    fn push(&mut self, frame: DepsFrame) {
        self.data.insert((frame, self.time));
        self.time += 1;
    }
    fn pop_most_constrained(&mut self) -> Option<(Summary, DepInfo)> {
        while let Some((mut frame, _)) = self.data.pop_first() {
            if let Some(sibling) = frame.remaining_siblings.next() {
                let parent = frame.parent.clone();
                self.data.insert((frame, self.time));
                self.time += 1;
                return Some((parent, sibling));
            }
        }
        None
    }
    fn iter(&self) -> impl Iterator<Item = (PackageId, &ResolvedDep, usize)> + '_ {
        self.data.iter().flat_map(|(frame, _)| {
            let parent = frame.parent.id.clone();
            frame
                .remaining_siblings
                .remaining()
                .map(move |info| (parent.clone(), &info.dep, info.candidates.len()))
        })
    }
}

/// The per-dep candidate iterator: consumes exactly one candidate per
/// call, skipping candidates whose semver group is already activated and
/// recording the reason (cargo `RemainingCandidates`; direct consumption
/// instead of cargo's peekable stash — each candidate is tried at most
/// once, so the activation loop provably terminates).
#[derive(Clone)]
struct RemainingCandidates {
    remaining: RcVecIter<Summary>,
}

impl RemainingCandidates {
    fn new(candidates: &Rc<Vec<Summary>>) -> Self {
        Self {
            remaining: RcVecIter::new(Rc::clone(candidates)),
        }
    }
    /// Returns the next activatable candidate and whether more remain.
    fn next(
        &mut self,
        conflicting_prev_active: &mut ConflictMap,
        activations: &BTreeMap<ActivationKey, (Summary, usize, bool)>,
    ) -> Option<(Summary, bool)> {
        let valid = |candidate: &Summary| match activations.get(&activation_key(&candidate.id)) {
            Some((a, _, _)) => a.id == candidate.id,
            None => true,
        };
        while let Some(b) = self.remaining.next() {
            let key = activation_key(&b.id);
            if let Some((a, _, _)) = activations.get(&key)
                && a.id != b.id
            {
                conflicting_prev_active
                    .entry(a.id.clone())
                    .or_insert(ConflictReason::Semver);
                continue;
            }
            // `has_another` must mean "another *activatable* candidate":
            // a saved frame is restored and its next() called against the
            // same activations, so a merely-present candidate that fails
            // the validity check would exhaust the restored frame and
            // break the "a saved frame always has a next" invariant.
            let has_another = self.remaining.remaining().any(valid);
            return Some((b, has_another));
        }
        None
    }
}

/// A saved state for backtracking (cargo `BacktrackFrame`).
struct BacktrackFrame {
    context: ResolverContext,
    remaining_deps: RemainingDeps,
    remaining_candidates: RemainingCandidates,
    parent: Summary,
    dep: ResolvedDep,
    conflicting_activations: ConflictMap,
}

/// The resolution state: activations and recorded edges.
/// The features requested on an activated package: the union over every
/// dependency edge into it (cargo's `resolve_features`).
#[derive(Clone, Default)]
struct RequestedFeatures {
    features: BTreeSet<String>,
    default_features: bool,
}

#[derive(Clone)]
struct ResolverContext {
    /// Number of decisions made (backjump target ages).
    age: usize,
    /// Activation key (name, semver-compat group, source) → summary + age.
    /// Lock-only registrations are inactive optional deps recorded for
    /// lockfile completeness; a later real activation upgrades them.
    activations: BTreeMap<ActivationKey, (Summary, usize, bool)>,
    /// Every resolved edge: (parent, dep, child).
    edges: Vec<(PackageId, ResolvedDep, PackageId, bool)>,
    /// Requested features per activated package (feature-aware optional
    /// deps, cargo style).
    requested: BTreeMap<PackageId, RequestedFeatures>,
}

impl ResolverContext {
    fn new() -> Self {
        Self {
            age: 0,
            activations: BTreeMap::new(),
            edges: Vec::new(),
            requested: BTreeMap::new(),
        }
    }
    fn is_active(&self, id: &PackageId) -> Option<usize> {
        self.activations
            .get(&activation_key(id))
            .and_then(|(s, age, _)| (s.id == *id).then_some(*age))
    }
    /// The newest age among `parent` and the conflict set, if all still
    /// active — the backjump target (cargo `is_conflicting`).
    fn is_conflicting(
        &self,
        parent: Option<&PackageId>,
        conflicting: &ConflictMap,
    ) -> Option<usize> {
        let mut max = 0;
        if let Some(parent) = parent {
            max = std::cmp::max(max, self.is_active(parent)?);
        }
        for id in conflicting.keys() {
            max = std::cmp::max(max, self.is_active(id)?);
        }
        Some(max)
    }
    /// Activates `summary`; `Err(Conflict)` when a semver-compatible
    /// version is already active. Returns `true` when already activated.
    /// A lock-only registration does not block a later real activation:
    /// the real one upgrades the entry and proceeds (cargo expands every
    /// activated package exactly once regardless of earlier lock-only
    /// sightings).
    fn flag_activated(
        &mut self,
        summary: &Summary,
        lock_only: bool,
    ) -> Result<bool, (PackageId, ConflictReason)> {
        let id = summary.id.clone();
        let age = self.age;
        let key = activation_key(&id);
        match self.activations.get(&key) {
            Some((a, _, is_lock_only)) => {
                if a.id != id {
                    return Err((a.id.clone(), ConflictReason::Semver));
                }
                if *is_lock_only && !lock_only {
                    // Upgrade: this package is really activated now; its
                    // deps must be expanded.
                    self.activations.insert(key, (summary.clone(), age, false));
                    return Ok(false);
                }
                Ok(true)
            }
            None => {
                self.activations
                    .insert(key, (summary.clone(), age, lock_only));
                Ok(false)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Conflict cache (ported from cargo src/resolver/conflict_cache.rs)
// ---------------------------------------------------------------------------

/// A trie of conflict sets: "this dependency cannot resolve while any of
/// these packages are active". Efficient subset search over all recorded
/// sets.
enum ConflictStoreTrie {
    Leaf(ConflictMap),
    Node(BTreeMap<PackageId, ConflictStoreTrie>),
}

impl ConflictStoreTrie {
    /// Finds a recorded conflict set whose members are all active, with the
    /// highest possible jump-back age.
    fn find(
        &self,
        is_active: &impl Fn(&PackageId) -> Option<usize>,
        must_contain: Option<&PackageId>,
        mut max_age: usize,
    ) -> Option<(&ConflictMap, usize)> {
        let mut out = None;
        match self {
            ConflictStoreTrie::Leaf(con) => {
                let age = con
                    .keys()
                    .filter(|id| must_contain.is_none_or(|must| *id == must))
                    .map(is_active)
                    .collect::<Option<BTreeSet<usize>>>()?
                    .into_iter()
                    .max()?;
                if age > max_age {
                    out = Some((con, age));
                }
            }
            ConflictStoreTrie::Node(children) => {
                for (id, child) in children {
                    let Some(age) = is_active(id) else { continue };
                    if age > max_age
                        && let Some(found) = child.find(is_active, must_contain, max_age)
                    {
                        max_age = found.1;
                        out = Some(found);
                    }
                }
            }
        }
        out
    }

    fn insert(&mut self, mut iter: impl Iterator<Item = PackageId>, con: ConflictMap) {
        match iter.next() {
            Some(id) => {
                let child = match self {
                    ConflictStoreTrie::Node(children) => children
                        .entry(id)
                        .or_insert_with(|| ConflictStoreTrie::Node(BTreeMap::new())),
                    ConflictStoreTrie::Leaf(_) => panic!("inserting into a leaf"),
                };
                child.insert(iter, con);
            }
            None => {
                *self = ConflictStoreTrie::Leaf(con);
            }
        }
    }
}

/// The global "past conflicts" cache (cargo `ConflictCache`).
#[derive(Default)]
struct ConflictCache {
    /// (dep name, req) → conflict-set trie.
    con_from_dep: BTreeMap<DepKey, ConflictStoreTrie>,
    /// Package → deps that mention it (inverse index).
    dep_from_pid: BTreeMap<PackageId, BTreeSet<DepKey>>,
}

/// Conflict-cache key for a dependency: name + req (string form —
/// `semver::VersionReq` has no `Ord`).
type DepKey = (String, String);

impl ConflictCache {
    fn insert(&mut self, dep: &ResolvedDep, conflicting: &ConflictMap) {
        let key = dep_key(dep);
        self.con_from_dep
            .entry(key.clone())
            .or_insert_with(|| ConflictStoreTrie::Node(BTreeMap::new()))
            .insert(conflicting.keys().cloned(), conflicting.clone());
        for id in conflicting.keys() {
            self.dep_from_pid
                .entry(id.clone())
                .or_default()
                .insert(key.clone());
        }
    }

    /// A conflict set for `dep` whose members are all active, if any.
    fn conflicting(&self, ctx: &ResolverContext, dep: &ResolvedDep) -> Option<&ConflictMap> {
        self.con_from_dep.get(&dep_key(dep)).and_then(|trie| {
            trie.find(&|id| ctx.is_active(id), None, 0)
                .map(|(con, _)| con)
        })
    }

    /// Conflict sets of deps that involve `pid` (used to prune frames whose
    /// deps are known unresolvable).
    fn dependencies_conflicting_with(&self, pid: &PackageId) -> Option<BTreeSet<DepKey>> {
        self.dep_from_pid.get(pid).cloned()
    }

    /// Finds a conflict set for `dep` that involves `pid` and whose other
    /// members are active.
    fn find_conflicting(
        &self,
        ctx: &ResolverContext,
        dep: &ResolvedDep,
        pid: &PackageId,
    ) -> Option<&ConflictMap> {
        self.con_from_dep.get(&dep_key(dep)).and_then(|trie| {
            trie.find(&|id| ctx.is_active(id), Some(pid), 0)
                .map(|(con, _)| con)
        })
    }
}

fn dep_key(dep: &ResolvedDep) -> DepKey {
    (
        dep.name.clone(),
        dep.req
            .as_ref()
            .map(|req| req.to_string())
            .unwrap_or_default(),
    )
}

// ---------------------------------------------------------------------------
// The resolver (ported from cargo src/resolver/mod.rs)
// ---------------------------------------------------------------------------

/// Resolves versions for `locals` (workspace/path packages) and their
/// registry dependencies, preferring `locked` versions and allowing their
/// yanked entries.
pub fn resolve(
    crates: &dyn CrateSource,
    locals: &[LocalPackage],
    locked: &TongLock,
    patched: &BTreeSet<String>,
) -> Result<Vec<ResolvedPackage>, ResolveError> {
    let t_resolve = std::time::Instant::now();
    let locals_map: BTreeMap<String, Summary> = locals
        .iter()
        .map(|local| (local.name.clone(), local_summary(local)))
        .collect();
    let mut queryer = Queryer {
        crates,
        locked,
        locals: &locals_map,
        patched,
        deps_cache: BTreeMap::new(),
    };

    let mut ctx = ResolverContext::new();
    let mut backtrack_stack: Vec<BacktrackFrame> = Vec::new();
    let mut remaining_deps = RemainingDeps::new();
    let mut past_conflicting = ConflictCache::default();

    // Activate all local packages to kick off the work.
    for local in locals {
        let summary = locals_map
            .get(&local.name)
            .expect("locals map covers every local")
            .clone();
        debug!(target: "tong::lock", phase = "resolve.activate_local", package = %local.name, version = %local.version);
        let frame =
            activate(&mut ctx, &mut queryer, None, summary, true).map_err(|err| match err {
                ActivateError::Fatal(err) => err,
                ActivateError::Conflict(id, reason) => ResolveError::Unresolvable {
                    chain: format!(
                        "cannot activate local package `{} v{}`: {:?}",
                        id.name, id.version, reason
                    ),
                },
            })?;
        if let Some(frame) = frame {
            remaining_deps.push(frame);
        }
    }

    let mut iterations: u64 = 0;
    while let Some((parent, info)) = remaining_deps.pop_most_constrained() {
        let dep = &info.dep;
        let candidates = &info.candidates;
        let expand = info.expand;
        iterations += 1;
        if iterations.is_multiple_of(50_000) {
            warn!(
                target: "tong::lock",
                phase = "resolve.guard",
                iterations,
                dep = %dep.name,
                parent = %parent.id.name,
                activations = ctx.activations.len(),
                pending = remaining_deps.data.len(),
                backtrack_frames = backtrack_stack.len(),
                duration_ms = t_resolve.elapsed().as_millis() as u64,
            );
        }

        let mut conflicting_activations = ConflictMap::new();
        let mut backtracked = false;
        let mut remaining_candidates = RemainingCandidates::new(candidates);

        loop {
            let next = remaining_candidates.next(&mut conflicting_activations, &ctx.activations);
            let (candidate, has_another) = match next {
                Some(tuple) => tuple,
                None => {
                    // All candidates exhausted: record the conflict set and
                    // backjump to the newest frame that can change it.
                    if !backtracked {
                        past_conflicting.insert(dep, &conflicting_activations);
                    }
                    match find_candidate(
                        &ctx,
                        &mut backtrack_stack,
                        &parent,
                        backtracked,
                        &conflicting_activations,
                    ) {
                        Some((candidate, has_another, frame)) => {
                            ctx = frame.context;
                            remaining_deps = frame.remaining_deps;
                            remaining_candidates = frame.remaining_candidates;
                            backtracked = true;
                            (candidate, has_another)
                        }
                        None => {
                            warn!(
                                target: "tong::lock",
                                phase = "resolve.failed",
                                package = %dep.name,
                                req = ?dep.req,
                                iterations,
                                duration_ms = t_resolve.elapsed().as_millis() as u64,
                            );
                            return Err(activation_error(&parent, dep, &conflicting_activations));
                        }
                    }
                }
            };

            let backtrack = if has_another {
                Some(BacktrackFrame {
                    context: ctx.clone(),
                    remaining_deps: remaining_deps.clone(),
                    remaining_candidates: remaining_candidates.clone(),
                    parent: parent.clone(),
                    dep: dep.clone(),
                    conflicting_activations: conflicting_activations.clone(),
                })
            } else {
                None
            };

            let chosen = ctx.activations.len();
            if chosen.is_multiple_of(25) {
                println!(
                    "  resolved {} packages so far ({} pending)",
                    chosen,
                    remaining_deps.data.len()
                );
            }
            info!(
                target: "tong::lock",
                phase = "resolve.choose",
                package = %dep.name,
                version = %candidate.id.version,
                chosen,
                pending = remaining_deps.data.len(),
                backtrack_frames = backtrack_stack.len(),
                duration_ms = t_resolve.elapsed().as_millis() as u64,
            );
            let res = activate(
                &mut ctx,
                &mut queryer,
                Some((&parent, dep)),
                candidate,
                expand,
            );

            // If any of our frame's deps are known unresolvable, we are too
            // (cargo's `has_past_conflicting_dep` pruning).
            let mut has_past_conflicting_dep = false;
            if let Ok(Some(ref frame)) = res {
                let pid = frame.parent.id.clone();
                if let Some(conflicting) = frame
                    .remaining_siblings
                    .remaining()
                    .find_map(|info: &DepInfo| past_conflicting.conflicting(&ctx, &info.dep))
                {
                    conflicting_activations.extend(
                        conflicting
                            .iter()
                            .filter(|&(p, _)| p != &pid)
                            .map(|(p, r)| (p.clone(), r.clone())),
                    );
                    has_past_conflicting_dep = true;
                }
                if !has_past_conflicting_dep
                    && let Some(known_related_bad_deps) =
                        past_conflicting.dependencies_conflicting_with(&pid)
                    && let Some((other_parent, conflict)) = remaining_deps
                        .iter()
                        .filter(|(_, other_dep, _)| {
                            known_related_bad_deps.contains(&dep_key(other_dep))
                        })
                        .filter_map(|(other_parent, other_dep, _)| {
                            past_conflicting
                                .find_conflicting(&ctx, other_dep, &pid)
                                .map(|con| (other_parent, con))
                        })
                        .next()
                {
                    let rel = conflict
                        .get(&pid)
                        .cloned()
                        .unwrap_or(ConflictReason::Semver);
                    conflicting_activations.extend(
                        conflict
                            .iter()
                            .filter(|&(p, _)| p != &pid)
                            .map(|(p, r)| (p.clone(), r.clone())),
                    );
                    conflicting_activations.insert(other_parent, rel);
                    has_past_conflicting_dep = true;
                }
            }

            let successfully_activated = match res {
                Ok(Some(frame)) => {
                    if !has_past_conflicting_dep {
                        remaining_deps.push(frame);
                        true
                    } else {
                        false
                    }
                }
                // Already activated: no extra work.
                Ok(None) => true,
                // Conflict: record the reason and try the next candidate.
                Err(ActivateError::Conflict(id, reason)) => {
                    conflicting_activations.insert(id, reason);
                    false
                }
                // Fatal (index failure, missing local edge): abort.
                Err(ActivateError::Fatal(err)) => return Err(err),
            };

            if successfully_activated {
                backtrack_stack.extend(backtrack);
                break;
            }

            // The failed activation may have mutated `ctx`; restore it.
            if let Some(b) = backtrack {
                ctx = b.context;
            }
        }
    }

    let mut out: Vec<ResolvedPackage> = ctx
        .activations
        .values()
        .map(|(summary, _, _)| {
            let mut dependencies: Vec<(String, Version, Option<String>)> = ctx
                .edges
                .iter()
                .filter(|(parent, _, _, _)| *parent == summary.id)
                .map(|(_, _, child, _)| {
                    let source = ctx
                        .activations
                        .get(&activation_key(child))
                        .and_then(|(child_summary, _, _)| child_summary.source.clone());
                    (child.name.clone(), child.version.clone(), source)
                })
                .collect();
            dependencies.sort();
            dependencies.dedup();
            ResolvedPackage {
                name: summary.id.name.clone(),
                version: summary.id.version.clone(),
                source: summary.source.clone(),
                checksum: summary.checksum.clone(),
                yanked: summary.yanked,
                local: summary.local,
                dependencies,
            }
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name).then(a.version.cmp(&b.version)));
    // Dev-dependency edges are excluded from the cycle walk: cargo permits
    // cycles that close through a dev edge (e.g. the tracing workspace:
    // tracing dev-depends on tracing-mock, which normal-depends back).
    // Dev-dependency edges and inactive optional (lock-only) edges are
    // excluded from the cycle walk: cargo permits cycles through dev
    // edges and never cycle-checks the lock-only closure (e.g. axum's
    // full optional closure contains spurious cycles through inactive
    // optional deps like rand's quickcheck).
    let cycle_edges: BTreeSet<(NodeId, NodeId)> = ctx
        .edges
        .iter()
        .filter(|(_, dep, _, expand)| !dep.dev && *expand)
        .map(|(parent, _, child, _)| {
            (
                (
                    parent.name.clone(),
                    parent.version.clone(),
                    parent.source.clone(),
                ),
                (
                    child.name.clone(),
                    child.version.clone(),
                    child.source.clone(),
                ),
            )
        })
        .collect();
    check_cycles(&out, &cycle_edges)?;
    println!("  resolved {} packages", out.len());
    info!(
        target: "tong::lock",
        phase = "resolve.total",
        packages = out.len(),
        iterations,
        duration_ms = t_resolve.elapsed().as_millis() as u64,
    );
    Ok(out)
}

/// The feature closure of a package's requested features (cargo
/// `build_requirements`): which optional deps are enabled, and which
/// features are requested on each enabled dep via `dep/feat` references.
struct FeatureClosure {
    enabled: BTreeSet<String>,
    /// Features seen in the closure (plain references): a dep whose name
    /// is seen is enabled via its implicit feature (crates.io's index v2
    /// omits implicit feature keys).
    seen: BTreeSet<String>,
    dep_features: BTreeMap<String, BTreeSet<String>>,
}

fn feature_closure(summary: &Summary, requested: Option<&RequestedFeatures>) -> FeatureClosure {
    let mut closure = FeatureClosure {
        enabled: BTreeSet::new(),
        seen: BTreeSet::new(),
        dep_features: BTreeMap::new(),
    };
    let Some(requested) = requested else {
        return closure;
    };
    let mut open: Vec<String> = requested.features.iter().cloned().collect();
    if requested.default_features {
        open.push("default".to_owned());
    }
    closure.seen = open.iter().cloned().collect();
    while let Some(feature) = open.pop() {
        let Some(references) = summary.features.get(&feature) else {
            // Implicit feature: the name matches an optional dependency
            // (crates.io index v2 omits implicit feature keys). Activating
            // it enables the dep (deadpool-runtime's `tokio_1` feature is
            // the implicit feature of its renamed optional tokio_1 dep).
            if summary.deps.iter().any(|dep| {
                dep.optional
                    && (dep.name == feature || dep.package.as_deref() == Some(feature.as_str()))
            }) {
                closure.enabled.insert(feature.clone());
            }
            continue;
        };
        for reference in references {
            if let Some(name) = reference.strip_prefix("dep:") {
                closure
                    .enabled
                    .insert(name.trim_end_matches('?').to_owned());
            } else if let Some((name, rest)) = reference.split_once('/') {
                // `name/feat` and weak `name?/feat` — activating a feature
                // of a dependency also activates the dependency itself and
                // requests `feat` on it (cargo `require_dep_feature`).
                closure
                    .enabled
                    .insert(name.trim_end_matches('?').to_owned());
                if !rest.is_empty() {
                    closure
                        .dep_features
                        .entry(name.trim_end_matches('?').to_owned())
                        .or_default()
                        .insert(rest.to_owned());
                }
            } else if closure.seen.insert(reference.clone()) {
                open.push(reference.clone());
            }
        }
    }
    closure
}

/// Attempts to activate `candidate`, returning its dependency frame when
/// newly activated. `Ok(None)` = already activated. `Err` = conflict.
/// The caller pushes the returned frame onto `remaining_deps`.
fn activate(
    ctx: &mut ResolverContext,
    queryer: &mut Queryer<'_>,
    parent: Option<(&Summary, &ResolvedDep)>,
    candidate: Summary,
    expand: bool,
) -> Result<Option<DepsFrame>, ActivateError> {
    ctx.age += 1;
    // Cargo's re-activation: a new request for a feature (or defaults)
    // this package has not seen before forces the deps to be recomputed
    // with the extended set (cargo `flag_activated`'s subset check +
    // `build_deps`). Without this, a package activated early (e.g. tokio
    // via axum's `time`) would keep the deps from the first edge only.
    let mut re_request = false;
    if let Some((parent_summary, dep)) = parent {
        ctx.edges.push((
            parent_summary.id.clone(),
            dep.clone(),
            candidate.id.clone(),
            expand,
        ));
        let requested = ctx.requested.entry(candidate.id.clone()).or_default();
        re_request = dep
            .features
            .iter()
            .any(|feature| !requested.features.contains(feature))
            || (dep.default_features && !requested.default_features);
        requested.features.extend(dep.features.iter().cloned());
        requested.default_features |= dep.default_features;
    }
    let already = ctx
        .flag_activated(&candidate, !expand)
        .map_err(|(id, reason)| ActivateError::Conflict(id, reason))?;
    if already && !re_request {
        return Ok(None);
    }
    // Dev-dependencies of registry packages are not locked (a documented
    // divergence from Cargo.lock completeness: registry dev-deps like
    // semver's `crates-index` pull enormous test-only closures). Local
    // packages lock theirs (cargo semantics for workspace members).
    // Feature-aware optional deps (cargo `build_deps`): an optional dep
    // is locked only when the requested features enable it.
    // `dep/feat` references inside the closure also request the dep's
    // feature (cargo `build_requirements`): tower's `log = ["tracing/log"]`
    // must request `log` on the tracing edge.
    let closure = feature_closure(&candidate, ctx.requested.get(&candidate.id));
    // Cargo locks every dependency of a resolved package — inactive
    // optional deps included (verified against real Cargo.lock files:
    // clap_builder's `anstream` and toml's `indexmap` appear even with
    // their features off). Only dev-dependencies of non-local packages
    // stay out. Inactive optional deps are version-locked and recorded as
    // edges but NOT expanded: their own optional closures do not cascade
    // (that is what keeps anyhow's lock at ~40 packages instead of the
    // full optional closure of everything reachable).
    //
    // A lock-only package's own dependencies are still recorded one
    // level (indexmap's non-optional `equivalent`/`hashbrown` edges
    // appear with `preserve_order` off) with the same closure filter
    // (indexmap's `arbitrary`/`borsh` optional deps stay out).
    if !expand {
        let deps: Vec<ResolvedDep> = candidate
            .deps
            .iter()
            .filter(|dep| !dep.dev || candidate.local)
            .filter(|dep| {
                !dep.optional
                    || closure.enabled.contains(&dep.name)
                    || closure.seen.contains(&dep.name)
            })
            .cloned()
            .collect();
        for dep in deps {
            let summaries = queryer
                .query(&candidate, &dep)
                .map_err(ActivateError::Fatal)?;
            if let Some(child) = summaries.first().cloned() {
                ctx.edges
                    .push((candidate.id.clone(), dep, child.id.clone(), false));
                ctx.flag_activated(&child, true)
                    .map_err(|(id, reason)| ActivateError::Conflict(id, reason))?;
            }
        }
        return Ok(None);
    }
    let mut deps: Vec<ResolvedDep> = candidate
        .deps
        .iter()
        .filter(|dep| !dep.dev || candidate.local)
        .filter(|dep| {
            !dep.optional || closure.enabled.contains(&dep.name) || closure.seen.contains(&dep.name)
        })
        .cloned()
        .collect();
    for dep in &mut deps {
        if let Some(features) = closure.dep_features.get(&dep.name) {
            for feature in features {
                if !dep.features.contains(feature) {
                    dep.features.push(feature.clone());
                }
            }
        }
    }
    let mut infos: Vec<DepInfo> = Vec::with_capacity(deps.len());
    for dep in deps {
        let summaries = queryer
            .query(&candidate, &dep)
            .map_err(ActivateError::Fatal)?;
        let expand = !dep.optional
            || closure.enabled.contains(&dep.name)
            || closure.seen.contains(&dep.name);
        infos.push(DepInfo {
            dep,
            candidates: summaries,
            expand,
        });
    }
    // Most constrained first (fewest candidates) — deterministic ties by
    // name via the DepsFrame ordering.
    infos.sort_by(|a, b| {
        a.candidates
            .len()
            .cmp(&b.candidates.len())
            .then_with(|| a.dep.name.cmp(&b.dep.name))
    });
    Ok(Some(DepsFrame {
        parent: candidate,
        remaining_siblings: RcVecIter::new(Rc::new(infos)),
    }))
}

type DependencyQueryKey = (String, Version, Option<String>, String, String);

/// The index query cache: candidates per (parent, dep).
struct Queryer<'a> {
    crates: &'a dyn CrateSource,
    locked: &'a TongLock,
    locals: &'a BTreeMap<String, Summary>,
    /// Crate names replaced by `[patch]` path/git entries: registry
    /// requirements on these names resolve to the patched local package.
    patched: &'a BTreeSet<String>,
    deps_cache: BTreeMap<DependencyQueryKey, Rc<Vec<Summary>>>,
}

impl Queryer<'_> {
    /// Candidate summaries for `dep` of `parent`: the local package for
    /// local edges, else versions matching the requirement — highest
    /// first, the locked version preferred, yanked only when locked.
    /// Cached per (parent, dep-name, requirement): two edges of one
    /// parent that ask for the same crate with different requirements
    /// (e.g. renames `alpha1`/`alpha2` at `=1.0.0`/`=2.0.0`) must see
    /// different candidate sets.
    fn query(
        &mut self,
        parent: &Summary,
        dep: &ResolvedDep,
    ) -> Result<Rc<Vec<Summary>>, ResolveError> {
        let crate_name = dep.package.as_deref().unwrap_or(&dep.name);
        let cache_key = (
            parent.id.name.clone(),
            parent.id.version.clone(),
            parent.id.source.clone(),
            crate_name.to_owned(),
            dep.req
                .as_ref()
                .map(|req| req.to_string())
                .unwrap_or_default(),
        );
        if let Some(cached) = self.deps_cache.get(&cache_key) {
            return Ok(Rc::clone(cached));
        }
        let summaries: Vec<Summary> = match &dep.req {
            None => {
                // Local edge: the target must be a local package.
                let Some(summary) = self.locals.get(crate_name) else {
                    return Err(ResolveError::Unresolvable {
                        chain: format!(
                            "local dependency `{}` of `{}` names no workspace/path package",
                            crate_name, parent.id.name
                        ),
                    });
                };
                vec![summary.clone()]
            }
            Some(req) => {
                // `[patch]`-replaced crates resolve to the patched local
                // package (serde's root patches `serde`/`serde_core`/
                // `serde_derive` to the workspace members; registry
                // packages' edges on those names must use the member).
                if self.patched.contains(crate_name)
                    && let Some(summary) = self
                        .locals
                        .get(crate_name)
                        .filter(|summary| req.matches(&summary.id.version))
                {
                    let summaries = Rc::new(vec![summary.clone()]);
                    self.deps_cache.insert(cache_key, Rc::clone(&summaries));
                    return Ok(summaries);
                }
                // The lock preference: any locked version that satisfies
                // the requirement — with several locked versions the
                // highest is preferred. Name-only lookups are gone: two
                // locked versions of one crate must not alias.
                let locked_parent = self.locked.candidates(&parent.id.name).find(|package| {
                    package.version == parent.id.version
                        && match &parent.id.source {
                            Some(source) => package.source == *source,
                            None => package.source.starts_with("registry+"),
                        }
                });
                let edge_locked_version = locked_parent.and_then(|package| {
                    package.dependencies.iter().find_map(|dependency| {
                        let (name, version, source) =
                            crate::lockfile::LockedPackage::parse_dependency(dependency);
                        (name == crate_name && source.starts_with("registry+"))
                            .then(|| Version::parse(version).ok())
                            .flatten()
                            .filter(|version| req.matches(version))
                    })
                });
                let locked_version = edge_locked_version.or_else(|| {
                    self.locked
                        .candidates(crate_name)
                        .filter(|package| package.source.starts_with("registry+"))
                        .filter(|package| req.matches(&package.version))
                        .map(|package| package.version.clone())
                        .max()
                });
                let mut versions: Vec<Summary> = self
                    .crates
                    .versions(crate_name)?
                    .into_iter()
                    .filter(|entry| req.matches(&entry.vers))
                    .filter(|entry| !entry.yanked || locked_version.as_ref() == Some(&entry.vers))
                    .map(|entry| summary_from_index(&entry))
                    .collect();
                // Highest first; the locked version preferred for
                // lockfile stability.
                versions.sort_by(|a, b| b.id.version.cmp(&a.id.version));
                if let Some(locked) = &locked_version
                    && req.matches(locked)
                    && let Some(index) = versions.iter().position(|s| s.id.version == *locked)
                {
                    let locked = versions.remove(index);
                    versions.insert(0, locked);
                }
                versions
            }
        };
        let summaries = Rc::new(summaries);
        self.deps_cache.insert(cache_key, Rc::clone(&summaries));
        Ok(summaries)
    }
}

fn summary_from_index(entry: &IndexVersion) -> Summary {
    Summary {
        id: PackageId::new(&entry.name, &entry.vers, None),
        deps: Rc::new(
            entry
                .deps
                .iter()
                .map(|dep| ResolvedDep {
                    name: dep.name.clone(),
                    package: dep.package.clone(),
                    req: Some(dep.req.clone()),
                    optional: dep.optional,
                    dev: dep.kind == IndexDepKind::Dev,
                    features: dep.features.clone(),
                    default_features: dep.default_features,
                })
                .collect(),
        ),
        features: entry.features.clone(),
        checksum: Some(entry.cksum.clone()),
        yanked: entry.yanked,
        local: false,
        source: None,
    }
}

fn local_summary(local: &LocalPackage) -> Summary {
    Summary {
        id: PackageId::new(&local.name, &local.version, local.source.clone()),
        deps: Rc::new(local.deps.clone()),
        features: BTreeMap::new(),
        checksum: None,
        yanked: false,
        local: true,
        source: local.source.clone(),
    }
}

/// Backjumps: pops backtrack frames until one can change the failed
/// outcome — its context predates the newest still-active conflict (cargo
/// `find_candidate`, without the #4834 conflict generalization).
fn find_candidate(
    ctx: &ResolverContext,
    backtrack_stack: &mut Vec<BacktrackFrame>,
    parent: &Summary,
    backtracked: bool,
    conflicting_activations: &ConflictMap,
) -> Option<(Summary, bool, BacktrackFrame)> {
    let age = if !backtracked {
        ctx.is_conflicting(Some(&parent.id), conflicting_activations)
    } else {
        None
    };
    let mut new_frame = None;
    if let Some(age) = age {
        while let Some(frame) = backtrack_stack.pop() {
            if !(frame.context.age >= age) {
                new_frame = Some(frame);
                break;
            }
            debug!(
                target: "tong::lock",
                phase = "resolve.backjump_skip",
                dep = %frame.dep.name,
                parent = %frame.parent.id.name,
                age = frame.context.age,
                target_age = age,
            );
        }
    } else {
        new_frame = backtrack_stack.pop();
    }
    new_frame.map(|mut frame| {
        let (candidate, has_another) = frame
            .remaining_candidates
            .next(
                &mut frame.conflicting_activations,
                &frame.context.activations,
            )
            .expect("a saved frame always has a next candidate");
        (candidate, has_another, frame)
    })
}

/// A diagnostic for an exhausted dependency (trimmed cargo
/// `errors::activation_error`).
fn activation_error(
    parent: &Summary,
    dep: &ResolvedDep,
    conflicting: &ConflictMap,
) -> ResolveError {
    let mut chain = format!(
        "failed to select a version for `{}` (required by {} v{})",
        dep.name, parent.id.name, parent.id.version
    );
    if !conflicting.is_empty() {
        let reasons: Vec<String> = conflicting
            .keys()
            .map(|id| format!("`{} v{}` is active", id.name, id.version))
            .collect();
        chain.push_str(&format!(
            "; conflicting activations: {}",
            reasons.join(", ")
        ));
    }
    ResolveError::Unresolvable { chain }
}

/// Cycle check over the resolved edges (ported cargo `check_cycles`).
///
/// Package identity here is `(name, version, source)`: a local `foo` and a
/// registry `foo` are different nodes, so edges can never attach to the
/// wrong one. The walk follows only non-dev edges (see the caller): cargo
/// permits cycles that close through a dev-dependency edge.
type NodeId = (String, Version, Option<String>);

fn check_cycles(
    packages: &[ResolvedPackage],
    cycle_edges: &BTreeSet<(NodeId, NodeId)>,
) -> Result<(), ResolveError> {
    let mut checked: BTreeSet<NodeId> = BTreeSet::new();
    let mut path: Vec<NodeId> = Vec::new();
    let mut visited: BTreeSet<NodeId> = BTreeSet::new();
    for pkg in packages {
        let id: NodeId = (pkg.name.clone(), pkg.version.clone(), pkg.source.clone());
        if !checked.contains(&id) {
            visit(cycle_edges, &id, &mut visited, &mut path, &mut checked)?;
        }
    }
    return Ok(());

    fn visit(
        cycle_edges: &BTreeSet<(NodeId, NodeId)>,
        id: &NodeId,
        visited: &mut BTreeSet<NodeId>,
        path: &mut Vec<NodeId>,
        checked: &mut BTreeSet<NodeId>,
    ) -> Result<(), ResolveError> {
        if !visited.insert(id.clone()) {
            let cycle: Vec<String> = path
                .iter()
                .rev()
                .take_while(|p| p != &id)
                .map(|p| format!("{} v{}", p.0, p.1))
                .collect();
            return Err(ResolveError::Unresolvable {
                chain: format!(
                    "cyclic package dependency: package `{} v{}` depends on itself (cycle: {})",
                    id.0,
                    id.1,
                    cycle.join(" -> ")
                ),
            });
        }
        if checked.insert(id.clone()) {
            path.push(id.clone());
            for (dep, version, source) in cycle_edges
                .iter()
                .filter(|(parent, _)| parent == id)
                .map(|(_, child)| child.clone())
            {
                visit(cycle_edges, &(dep, version, source), visited, path, checked)?;
            }
            path.pop();
        }
        visited.remove(id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sparse_index::{IndexDep, IndexDepKind};
    use std::collections::BTreeMap;

    /// A fixture index: name → entries.
    struct Fixture(BTreeMap<String, Vec<IndexVersion>>);

    impl Fixture {
        fn entry(name: &str, vers: &str, deps: &[(&str, &str)], yanked: bool) -> IndexVersion {
            IndexVersion {
                name: name.to_owned(),
                vers: Version::parse(vers).unwrap(),
                deps: deps
                    .iter()
                    .map(|(dep, req)| IndexDep {
                        name: (*dep).to_owned(),
                        req: VersionReq::parse(req).unwrap(),
                        features: Vec::new(),
                        optional: false,
                        default_features: true,
                        target: None,
                        kind: IndexDepKind::Normal,
                        package: None,
                    })
                    .collect(),
                cksum: format!("{name}-{vers}"),
                features: BTreeMap::new(),
                features2: None,
                rust_version: None,
                yanked,
                v: 1,
            }
        }
    }

    impl CrateSource for Fixture {
        fn versions(&self, name: &str) -> Result<Vec<IndexVersion>, crate::FetchError> {
            Ok(self.0.get(name).cloned().unwrap_or_default())
        }
    }

    fn edge(name: &str, req: &str) -> ResolvedDep {
        ResolvedDep {
            name: name.to_owned(),
            package: None,
            req: Some(VersionReq::parse(req).unwrap()),
            optional: false,
            dev: false,
            features: Vec::new(),
            default_features: true,
        }
    }

    fn root(deps: Vec<ResolvedDep>) -> LocalPackage {
        LocalPackage {
            name: "root".to_owned(),
            version: Version::new(0, 1, 0),
            source: Some("path+.".to_owned()),
            deps,
        }
    }

    fn names(packages: &[ResolvedPackage], name: &str) -> Vec<String> {
        packages
            .iter()
            .filter(|p| p.name == name)
            .map(|p| p.version.to_string())
            .collect()
    }

    /// The `syn 2.x + syn 3.x` case that hung the old one-version-per-name
    /// resolver: two parents require semver-incompatible versions of the
    /// same crate — both must coexist in the lockfile.
    #[test]
    fn semver_incompatible_versions_coexist() {
        let fixture = Fixture(BTreeMap::from([
            (
                "syn".to_owned(),
                vec![
                    Fixture::entry("syn", "2.0.0", &[], false),
                    Fixture::entry("syn", "3.0.3", &[], false),
                ],
            ),
            (
                "tokio-macros".to_owned(),
                vec![Fixture::entry(
                    "tokio-macros",
                    "2.7.2",
                    &[("syn", "^3")],
                    false,
                )],
            ),
            (
                "matchers".to_owned(),
                vec![Fixture::entry("matchers", "0.1.0", &[("syn", "^2")], false)],
            ),
        ]));
        let packages = resolve(
            &fixture,
            &[root(vec![edge("tokio-macros", "*"), edge("matchers", "*")])],
            &TongLock::default(),
            &BTreeSet::new(),
        )
        .unwrap();
        let syn = names(&packages, "syn");
        assert!(syn.contains(&"2.0.0".to_owned()), "{syn:?}");
        assert!(syn.contains(&"3.0.3".to_owned()), "{syn:?}");
        // Each parent's edge pins the exact version it resolved to.
        let macros = packages.iter().find(|p| p.name == "tokio-macros").unwrap();
        assert_eq!(
            macros.dependencies,
            vec![("syn".to_owned(), Version::new(3, 0, 3), None)]
        );
        let matchers = packages.iter().find(|p| p.name == "matchers").unwrap();
        assert_eq!(
            matchers.dependencies,
            vec![("syn".to_owned(), Version::new(2, 0, 0), None)]
        );
    }

    /// A genuinely unresolvable same-group conflict must terminate with an
    /// error (the old resolver spun forever on similar graphs).
    #[test]
    fn unresolvable_conflict_terminates() {
        let fixture = Fixture(BTreeMap::from([
            (
                "a".to_owned(),
                vec![
                    Fixture::entry("a", "1.0.0", &[("c", "~1.0")], false),
                    Fixture::entry("a", "1.0.1", &[("c", "~1.0")], false),
                    Fixture::entry("a", "1.0.2", &[("c", "~1.0")], false),
                ],
            ),
            (
                "b".to_owned(),
                vec![Fixture::entry("b", "1.0.0", &[("c", "^1.1")], false)],
            ),
            (
                "c".to_owned(),
                vec![
                    Fixture::entry("c", "1.0.0", &[], false),
                    Fixture::entry("c", "1.1.0", &[], false),
                    Fixture::entry("c", "1.2.0", &[], false),
                ],
            ),
        ]));
        // a picks c ~1.0 (only 1.0.0 matches); b requires c ^1.1 — both
        // live in Major(1), so no version satisfies both; backtracking
        // must exhaust and fail.
        let err = resolve(
            &fixture,
            &[root(vec![edge("a", "*"), edge("b", "*")])],
            &TongLock::default(),
            &BTreeSet::new(),
        )
        .unwrap_err();
        assert!(matches!(err, ResolveError::Unresolvable { .. }), "{err}");
    }

    /// Backtracking finds the satisfying combination when it exists.
    #[test]
    fn backtracks_to_satisfy_all_requirements() {
        let fixture = Fixture(BTreeMap::from([
            (
                "a".to_owned(),
                vec![Fixture::entry("a", "2.0.0", &[("c", "^2")], false)],
            ),
            (
                "b".to_owned(),
                vec![Fixture::entry("b", "1.0.0", &[("c", "^1")], false)],
            ),
            (
                "c".to_owned(),
                vec![
                    Fixture::entry("c", "1.0.0", &[], false),
                    Fixture::entry("c", "2.0.0", &[], false),
                ],
            ),
        ]));
        let packages = resolve(
            &fixture,
            &[root(vec![edge("a", "*"), edge("b", "*")])],
            &TongLock::default(),
            &BTreeSet::new(),
        )
        .unwrap();
        // a requires c ^2 → c 2.0.0; b requires c ^1 → c 1.0.0. Both
        // coexist (different major groups).
        let c = names(&packages, "c");
        assert!(c.contains(&"1.0.0".to_owned()), "{c:?}");
        assert!(c.contains(&"2.0.0".to_owned()), "{c:?}");
    }

    /// The exact locked parent edge wins over other compatible locks.
    #[test]
    fn locked_parent_edge_is_preferred() {
        let fixture = Fixture(BTreeMap::from([(
            "alpha".to_owned(),
            vec![
                Fixture::entry("alpha", "1.0.0", &[], false),
                Fixture::entry("alpha", "1.2.0", &[], false),
                Fixture::entry("alpha", "1.5.0", &[], false),
            ],
        )]));
        let locked = TongLock {
            version: 1,
            packages: vec![
                crate::lockfile::LockedPackage {
                    name: "root".to_owned(),
                    version: Version::new(0, 1, 0),
                    source: "path+.".to_owned(),
                    checksum: None,
                    manifest_checksum: None,
                    tree_digest: None,
                    yanked: false,
                    publish_time: None,
                    dependencies: vec!["alpha 1.0.0 registry+fixture".to_owned()],
                },
                crate::lockfile::LockedPackage {
                    name: "alpha".to_owned(),
                    version: Version::new(1, 2, 0),
                    source: "registry+fixture".to_owned(),
                    checksum: Some("x".to_owned()),
                    manifest_checksum: None,
                    tree_digest: None,
                    yanked: false,
                    publish_time: None,
                    dependencies: Vec::new(),
                },
                crate::lockfile::LockedPackage {
                    name: "alpha".to_owned(),
                    version: Version::new(1, 5, 0),
                    source: "path+workspace-alpha".to_owned(),
                    checksum: None,
                    manifest_checksum: None,
                    tree_digest: None,
                    yanked: false,
                    publish_time: None,
                    dependencies: Vec::new(),
                },
            ],
        };
        let packages = resolve(
            &fixture,
            &[root(vec![edge("alpha", "^1")])],
            &locked,
            &BTreeSet::new(),
        )
        .unwrap();
        let alpha = packages.iter().find(|p| p.name == "alpha").unwrap();
        assert_eq!(alpha.version.to_string(), "1.0.0");
    }

    /// Yanked versions are excluded unless already locked.
    #[test]
    fn yanked_versions_require_a_lock_entry() {
        let fixture = Fixture(BTreeMap::from([(
            "gamma".to_owned(),
            vec![
                Fixture::entry("gamma", "1.0.0", &[], true),
                Fixture::entry("gamma", "1.1.0", &[], false),
            ],
        )]));
        // No lock: the yanked 1.0.0 is excluded.
        let packages = resolve(
            &fixture,
            &[root(vec![edge("gamma", "*")])],
            &TongLock::default(),
            &BTreeSet::new(),
        )
        .unwrap();
        assert_eq!(names(&packages, "gamma"), vec!["1.1.0"]);
        // Locked yanked version is allowed and preferred.
        let locked = TongLock {
            version: 1,
            packages: vec![crate::lockfile::LockedPackage {
                name: "gamma".to_owned(),
                version: Version::new(1, 0, 0),
                source: "registry+fixture".to_owned(),
                checksum: Some("x".to_owned()),
                manifest_checksum: None,
                tree_digest: None,
                yanked: true,
                publish_time: None,
                dependencies: Vec::new(),
            }],
        };
        let packages = resolve(
            &fixture,
            &[root(vec![edge("gamma", "*")])],
            &locked,
            &BTreeSet::new(),
        )
        .unwrap();
        assert_eq!(names(&packages, "gamma"), vec!["1.0.0"]);
    }

    /// 0.x compatibility: `0.1.x` and `0.2.x` coexist; `0.0.x` is
    /// per-patch.
    #[test]
    fn zero_major_compatibility_groups() {
        let fixture = Fixture(BTreeMap::from([
            (
                "z".to_owned(),
                vec![
                    Fixture::entry("z", "0.1.5", &[], false),
                    Fixture::entry("z", "0.2.0", &[], false),
                    Fixture::entry("z", "0.0.2", &[], false),
                    Fixture::entry("z", "0.0.1", &[], false),
                ],
            ),
            (
                "u".to_owned(),
                vec![Fixture::entry("u", "1.0.0", &[("z", "^0.1")], false)],
            ),
            (
                "v".to_owned(),
                vec![Fixture::entry("v", "1.0.0", &[("z", "^0.2")], false)],
            ),
            (
                "w".to_owned(),
                vec![Fixture::entry("w", "1.0.0", &[("z", "=0.0.1")], false)],
            ),
        ]));
        let packages = resolve(
            &fixture,
            &[root(vec![edge("u", "*"), edge("v", "*"), edge("w", "*")])],
            &TongLock::default(),
            &BTreeSet::new(),
        )
        .unwrap();
        let z = names(&packages, "z");
        assert!(z.contains(&"0.1.5".to_owned()), "{z:?}");
        assert!(z.contains(&"0.2.0".to_owned()), "{z:?}");
        assert!(z.contains(&"0.0.1".to_owned()), "{z:?}");
        assert!(!z.contains(&"0.0.2".to_owned()), "{z:?}");
    }

    /// A dev-dependency cycle (the real serde ↔ serde_core layout,
    /// verified against the live index: serde_core's `serde ^1` edge is a
    /// dev-dependency) must not create a graph cycle and resolves.
    #[test]
    fn dev_dependency_cycles_do_not_cycle() {
        let fixture = Fixture(BTreeMap::from([
            (
                "serde".to_owned(),
                vec![Fixture::entry(
                    "serde",
                    "1.0.229",
                    &[("serde_core", "^1"), ("serde_derive", "=1.0.229")],
                    false,
                )],
            ),
            (
                "serde_core".to_owned(),
                vec![IndexVersion {
                    name: "serde_core".to_owned(),
                    vers: Version::parse("1.0.229").unwrap(),
                    deps: vec![
                        IndexDep {
                            name: "serde".to_owned(),
                            req: VersionReq::parse("^1").unwrap(),
                            features: Vec::new(),
                            optional: false,
                            default_features: true,
                            target: None,
                            kind: IndexDepKind::Dev,
                            package: None,
                        },
                        IndexDep {
                            name: "serde_derive".to_owned(),
                            req: VersionReq::parse("=1.0.229").unwrap(),
                            features: Vec::new(),
                            optional: false,
                            default_features: true,
                            target: None,
                            kind: IndexDepKind::Normal,
                            package: None,
                        },
                    ],
                    cksum: "serde_core-1.0.229".to_owned(),
                    features: BTreeMap::new(),
                    features2: None,
                    rust_version: None,
                    yanked: false,
                    v: 1,
                }],
            ),
            (
                "serde_derive".to_owned(),
                vec![Fixture::entry("serde_derive", "1.0.229", &[], false)],
            ),
        ]));
        let packages = resolve(
            &fixture,
            &[root(vec![edge("serde", "*"), edge("serde_derive", "^1")])],
            &TongLock::default(),
            &BTreeSet::new(),
        )
        .unwrap();
        assert_eq!(names(&packages, "serde"), vec!["1.0.229"]);
        assert_eq!(names(&packages, "serde_core"), vec!["1.0.229"]);
        assert_eq!(names(&packages, "serde_derive"), vec!["1.0.229"]);
        // serde_core's dev edge must not appear in its lock entry.
        let core = packages.iter().find(|p| p.name == "serde_core").unwrap();
        assert_eq!(
            core.dependencies,
            vec![("serde_derive".to_owned(), Version::new(1, 0, 229), None)]
        );
    }

    /// A genuine normal-dependency cycle is rejected after resolution
    /// (cargo `check_cycles`).
    #[test]
    fn normal_dependency_cycles_are_rejected() {
        let fixture = Fixture(BTreeMap::from([
            (
                "a".to_owned(),
                vec![Fixture::entry("a", "1.0.0", &[("b", "^1")], false)],
            ),
            (
                "b".to_owned(),
                vec![Fixture::entry("b", "1.0.0", &[("a", "^1")], false)],
            ),
        ]));
        let err = resolve(
            &fixture,
            &[root(vec![edge("a", "*")])],
            &TongLock::default(),
            &BTreeSet::new(),
        )
        .unwrap_err();
        assert!(
            matches!(err, ResolveError::Unresolvable { .. }) && err.to_string().contains("cyclic"),
            "{err}"
        );
    }

    /// Registry packages lock feature-activated optional edges.
    #[test]
    fn optional_deps_lock_semantics_match_cargo() {
        // A package with an optional dep enabled only by a non-default
        // feature, and a default-feature optional dep.
        let with_optional = IndexVersion {
            name: "pkg".to_owned(),
            vers: Version::parse("1.0.0").unwrap(),
            deps: vec![
                IndexDep {
                    name: "base".to_owned(),
                    req: VersionReq::parse("^1").unwrap(),
                    features: Vec::new(),
                    optional: false,
                    default_features: true,
                    target: None,
                    kind: IndexDepKind::Normal,
                    package: None,
                },
                IndexDep {
                    name: "extra".to_owned(),
                    req: VersionReq::parse("^1").unwrap(),
                    features: Vec::new(),
                    optional: true,
                    default_features: true,
                    target: None,
                    kind: IndexDepKind::Normal,
                    package: None,
                },
            ],
            cksum: "pkg-1.0.0".to_owned(),
            features: BTreeMap::from([
                ("default".to_owned(), vec!["base".to_owned()]),
                ("extra".to_owned(), vec!["extra".to_owned()]),
            ]),
            features2: None,
            rust_version: None,
            yanked: false,
            v: 1,
        };
        let fixture = Fixture(BTreeMap::from([
            ("pkg".to_owned(), vec![with_optional]),
            (
                "base".to_owned(),
                vec![Fixture::entry("base", "1.0.0", &[], false)],
            ),
            (
                "extra".to_owned(),
                vec![Fixture::entry("extra", "1.0.0", &[], false)],
            ),
        ]));

        // Default features only: inactive `extra` stays out.
        let packages = resolve(
            &fixture,
            &[root(vec![edge("pkg", "^1")])],
            &TongLock::default(),
            &BTreeSet::new(),
        )
        .unwrap();
        let pkg = packages.iter().find(|p| p.name == "pkg").unwrap();
        assert_eq!(
            pkg.dependencies,
            vec![("base".to_owned(), Version::new(1, 0, 0), None)]
        );

        // The `extra` feature requested on the edge: `extra` is locked.
        let mut deps = vec![edge("pkg", "^1")];
        deps[0].features.push("extra".to_owned());
        let packages = resolve(
            &fixture,
            &[root(deps)],
            &TongLock::default(),
            &BTreeSet::new(),
        )
        .unwrap();
        let pkg = packages.iter().find(|p| p.name == "pkg").unwrap();
        assert_eq!(
            pkg.dependencies,
            vec![
                ("base".to_owned(), Version::new(1, 0, 0), None),
                ("extra".to_owned(), Version::new(1, 0, 0), None),
            ]
        );
    }

    /// A local package locks dev-dependencies and feature-active optional
    /// edges.
    #[test]
    fn local_package_lock_semantics_match_cargo() {
        let with_optional = IndexVersion {
            name: "pkg".to_owned(),
            vers: Version::parse("1.0.0").unwrap(),
            deps: vec![
                IndexDep {
                    name: "base".to_owned(),
                    req: VersionReq::parse("^1").unwrap(),
                    features: Vec::new(),
                    optional: false,
                    default_features: true,
                    target: None,
                    kind: IndexDepKind::Normal,
                    package: None,
                },
                IndexDep {
                    name: "extra".to_owned(),
                    req: VersionReq::parse("^1").unwrap(),
                    features: Vec::new(),
                    optional: true,
                    default_features: true,
                    target: None,
                    kind: IndexDepKind::Normal,
                    package: None,
                },
            ],
            cksum: "pkg-1.0.0".to_owned(),
            features: BTreeMap::from([("default".to_owned(), vec!["base".to_owned()])]),
            features2: None,
            rust_version: None,
            yanked: false,
            v: 1,
        };
        let fixture = Fixture(BTreeMap::from([
            ("pkg".to_owned(), vec![with_optional]),
            (
                "base".to_owned(),
                vec![Fixture::entry("base", "1.0.0", &[], false)],
            ),
            (
                "extra".to_owned(),
                vec![Fixture::entry("extra", "1.0.0", &[], false)],
            ),
        ]));
        // The inactive optional edge stays out; the dev dependency is
        // locked for the workspace member.
        let mut root_deps = vec![edge("pkg", "^1")];
        root_deps.push(ResolvedDep {
            name: "extra".to_owned(),
            package: None,
            req: Some(VersionReq::parse("^1").unwrap()),
            optional: true,
            dev: false,
            features: Vec::new(),
            default_features: true,
        });
        root_deps.push(ResolvedDep {
            name: "base".to_owned(),
            package: None,
            req: Some(VersionReq::parse("^1").unwrap()),
            optional: false,
            dev: true,
            features: Vec::new(),
            default_features: true,
        });
        let packages = resolve(
            &fixture,
            &[root(root_deps)],
            &TongLock::default(),
            &BTreeSet::new(),
        )
        .unwrap();
        let root = packages.iter().find(|p| p.name == "root").unwrap();
        assert_eq!(
            root.dependencies,
            vec![
                ("base".to_owned(), Version::new(1, 0, 0), None),
                ("pkg".to_owned(), Version::new(1, 0, 0), None),
            ]
        );
        let pkg = packages.iter().find(|p| p.name == "pkg").unwrap();
        assert_eq!(
            pkg.dependencies,
            vec![("base".to_owned(), Version::new(1, 0, 0), None)]
        );
    }

    /// Local packages activate at their exact versions and local edges
    /// resolve without index queries.
    #[test]
    fn local_packages_and_edges() {
        let fixture = Fixture(BTreeMap::new());
        let packages = resolve(
            &fixture,
            &[
                LocalPackage {
                    name: "app".to_owned(),
                    version: Version::new(0, 1, 0),
                    source: Some("path+crates/app".to_owned()),
                    deps: vec![ResolvedDep {
                        name: "core".to_owned(),
                        package: None,
                        req: None,
                        optional: false,
                        dev: false,
                        features: Vec::new(),
                        default_features: true,
                    }],
                },
                LocalPackage {
                    name: "core".to_owned(),
                    version: Version::new(0, 2, 0),
                    source: Some("path+crates/core".to_owned()),
                    deps: Vec::new(),
                },
            ],
            &TongLock::default(),
            &BTreeSet::new(),
        )
        .unwrap();
        let app = packages.iter().find(|p| p.name == "app").unwrap();
        assert_eq!(
            app.dependencies,
            vec![(
                "core".to_owned(),
                Version::new(0, 2, 0),
                Some("path+crates/core".to_owned())
            )]
        );
        assert!(app.local);
        assert_eq!(app.source.as_deref(), Some("path+crates/app"));
    }

    /// The output is deterministic.
    #[test]
    fn resolution_is_deterministic() {
        let fixture = Fixture(BTreeMap::from([
            (
                "a".to_owned(),
                vec![Fixture::entry("a", "1.0.0", &[("c", "^1")], false)],
            ),
            (
                "b".to_owned(),
                vec![Fixture::entry("b", "1.0.0", &[("c", "^1")], false)],
            ),
            (
                "c".to_owned(),
                vec![Fixture::entry("c", "1.0.0", &[], false)],
            ),
        ]));
        let deps = vec![edge("a", "*"), edge("b", "*")];
        let first = resolve(
            &fixture,
            &[root(deps.clone())],
            &TongLock::default(),
            &BTreeSet::new(),
        )
        .unwrap();
        let second = resolve(
            &fixture,
            &[root(deps)],
            &TongLock::default(),
            &BTreeSet::new(),
        )
        .unwrap();
        assert_eq!(first, second);
    }
}
