//! Cargo-compatible feature resolution (resolver 1/2/3 semantics).
//!
//! Pure module, no I/O; deterministic (BTreeMap/BTreeSet everywhere).
//! Implements the resolver rules: weak dep features (`pkg?/feat`),
//! namespaced `dep:` features, optional deps → implicit `dep_name`
//! features, the `default` feature, `default-features = false`, and
//! dependency feature propagation via `dep/feat` with Cargo's union
//! (unification) semantics.
//!
//! Feature domains (Cargo's resolver versions, selected per workspace):
//!
//! - Resolver 1: one domain — features unify across normal, build, and
//!   dev dependencies (the legacy behavior that made `cargo build` see
//!   dev-dep features).
//! - Resolver 2: dev-dependencies are a separate domain; normal and
//!   build-dependencies unify.
//! - Resolver 3: build-dependencies form a separate *host* domain too — a
//!   package used as both a normal and a build dependency keeps
//!   independent feature sets (`FeatureMap::build_features`).
//!
//! The resolution is a fixpoint workqueue: activating a feature enqueues
//! the references it declares; every (package, feature, domain) triple is
//! processed at most once, so the algorithm always terminates.

use std::collections::{BTreeMap, BTreeSet};

use tong_core::canonical::{CanonicalEncode, Encoder};

use crate::model::{Dep, Package, PackageId, ResolverVersion, RustModel};

/// A feature request for a workspace package (CLI `--features`,
/// `--no-default-features`, `--all-features`, or `Tong.toml` target
/// `features`).
#[derive(Clone, Debug)]
pub struct FeatureRequest {
    /// Exact identity of the package to activate.
    pub package: PackageId,
    /// Features to activate.
    pub features: Vec<String>,
    /// Whether the package's default feature is enabled.
    pub default_features: bool,
}

/// The resolved feature activation state of the whole graph.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FeatureMap {
    /// Activated features per package (target domain, sorted).
    pub packages: BTreeMap<PackageId, BTreeSet<String>>,
    /// Active optional dep edges per package in the target domain:
    /// extern name of the dep. Needed because an activated optional dep
    /// may carry no features of its own (e.g. `default-features = false`
    /// with an empty list), in which case the package's feature set is
    /// empty but the edge is live.
    pub active_optional_deps: BTreeMap<PackageId, BTreeSet<String>>,
    /// Resolver-3 host domain: features activated on build-dependency
    /// packages (compiled for the execution host). Empty under
    /// resolvers 1 and 2, where build edges share the target domain.
    pub build_features: BTreeMap<PackageId, BTreeSet<String>>,
    /// Active optional build-dep edges per package (host domain).
    pub active_build_optional_deps: BTreeMap<PackageId, BTreeSet<String>>,
}

impl FeatureMap {
    /// The activated features of `package` in the given domain; the host
    /// domain falls back to the unified map (resolvers 1/2 have no host
    /// domain).
    pub fn features_for(&self, package: &PackageId, host: bool) -> &BTreeSet<String> {
        if host {
            self.build_features
                .get(package)
                .unwrap_or_else(|| self.packages.get(package).unwrap_or(&EMPTY_FEATURES))
        } else {
            self.packages.get(package).unwrap_or(&EMPTY_FEATURES)
        }
    }

    /// Whether the optional edge `extern_name` of `package` is active in
    /// either domain (build edges live in the host domain under resolver
    /// 3).
    pub fn edge_active(&self, package: &PackageId, extern_name: &str) -> bool {
        self.active_optional_deps
            .get(package)
            .is_some_and(|active| active.contains(extern_name))
            || self
                .active_build_optional_deps
                .get(package)
                .is_some_and(|active| active.contains(extern_name))
    }
}

static EMPTY_FEATURES: BTreeSet<String> = BTreeSet::new();

impl CanonicalEncode for FeatureMap {
    fn encode(&self, enc: &mut Encoder) {
        encode_domain(enc, &self.packages);
        encode_domain(enc, &self.active_optional_deps);
        encode_domain(enc, &self.build_features);
        encode_domain(enc, &self.active_build_optional_deps);
    }
}

fn encode_domain(enc: &mut Encoder, domain: &BTreeMap<PackageId, BTreeSet<String>>) {
    enc.write_u64(domain.len() as u64);
    for (package, features) in domain {
        package.encode(enc);
        enc.write_seq(&features.iter().cloned().collect::<Vec<_>>());
    }
}

/// Feature resolution failure, with Cargo-style messages.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FeatureError {
    /// A feature name does not exist on the package.
    UnknownFeature { package: String, feature: String },
    /// A `dep/feat` or `dep?/feat` reference names a missing dep feature.
    UnknownDepFeature {
        package: String,
        feature: String,
        dep: String,
    },
    /// A reference names a dependency that does not exist.
    UnknownDep { package: String, dep: String },
    /// A `dep:` or plain reference names a non-optional dependency.
    NotOptionalDep {
        package: String,
        feature: String,
        dep: String,
    },
    /// A request or edge references a package that is not in the model.
    UnknownPackage(String),
}

impl std::fmt::Display for FeatureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownFeature { package, feature } => {
                write!(
                    f,
                    "the package `{package}` does not contain the feature `{feature}`"
                )
            }
            Self::UnknownDepFeature {
                package,
                feature,
                dep,
            } => write!(
                f,
                "the package `{package}` does not contain the feature `{feature}` \
                 of dependency `{dep}`"
            ),
            Self::UnknownDep { package, dep } => write!(
                f,
                "the package `{package}` references dependency `{dep}`, which is not \
                 a dependency of it"
            ),
            Self::NotOptionalDep {
                package,
                feature,
                dep,
            } => write!(
                f,
                "feature `{feature}` of package `{package}` includes `{dep}`, but `{dep}` \
                 is not an optional dependency"
            ),
            Self::UnknownPackage(name) => {
                write!(f, "feature resolution references unknown package `{name}`")
            }
        }
    }
}

impl std::error::Error for FeatureError {}

/// Resolves the feature activation state of the whole model under the
/// model's selected resolver version.
///
/// `requests` seed the workspace members (default: all members with
/// `default_features: true`). Non-optional dep edges of every package are
/// always active within their domain; optional edges activate only through
/// features. `include_dev_deps` additionally treats dev-dep edges as
/// active in the target domain (test builds; dev-deps are excluded from
/// normal builds, like Cargo; resolver 1 unifies them regardless).
pub fn resolve_features(
    model: &RustModel,
    requests: &[FeatureRequest],
    include_dev_deps: bool,
) -> Result<FeatureMap, FeatureError> {
    let packages: BTreeMap<PackageId, &Package> = model
        .packages
        .iter()
        .map(|package| (package.id.clone(), package))
        .collect();

    // cc_import targets are native imports, not feature-bearing crates:
    // edges referencing them are ignored (they never participate in
    // features).
    let native_imports: BTreeSet<&str> = model
        .cc_imports
        .iter()
        .map(|import| import.name.as_str())
        .collect();

    let mut state = Resolver {
        resolver: model.resolver,
        include_dev: include_dev_deps,
        packages: &packages,
        features_on: packages
            .keys()
            .map(|id| (id.clone(), BTreeSet::new()))
            .collect(),
        active_optional: BTreeMap::new(),
        host_features_on: packages
            .keys()
            .map(|id| (id.clone(), BTreeSet::new()))
            .collect(),
        host_active_optional: BTreeMap::new(),
        default_on: BTreeMap::new(),
        host_default_on: BTreeMap::new(),
        queue: Vec::new(),
        edge_queue: Vec::new(),
        pending_weak: Vec::new(),
        expanded: BTreeSet::new(),
        native_imports: &native_imports,
    };

    // Seed: explicit workspace-member requests (target domain). Expansion
    // (non-optional edges of a package activate when the package is first
    // reached in a domain) covers the rest of the graph — members reach
    // their deps, deps reach their deps, in the domain the edge belongs
    // to.
    for request in requests {
        state.ensure_expanded(&request.package, Domain::Target);
        if request.default_features {
            state.mark_default(&request.package, Domain::Target);
        }
        for feature in &request.features {
            state
                .queue
                .push((request.package.clone(), feature.clone(), Domain::Target));
        }
    }

    // Resolver 3 host domain: packages with build scripts compile their
    // (non-optional) build-dependencies for the host — a separate feature
    // domain from the target one.
    if state.resolver == ResolverVersion::V3 {
        for package in &model.packages {
            if package.build_script.is_some() {
                for dep in &package.build_deps {
                    if !dep.optional && !state.native_imports.contains(dep.package.name.as_str()) {
                        state.activate_edge(&package.id, dep, Domain::Host)?;
                    }
                }
            }
        }
    }

    // Fixpoint: process queued (package, feature, domain) activations,
    // queued edge activations (a package first reached in a domain
    // expands its non-optional edges there), then re-check deferred weak
    // refs whose dep became active meanwhile. Any new queue work — from
    // feature processing or edge expansion — continues the loop.
    loop {
        let mut progress = false;
        while let Some((package, feature, domain)) = state.queue.pop() {
            state.process(&package, &feature, domain)?;
            progress = true;
        }
        while let Some((parent, dep, domain)) = state.edge_queue.pop() {
            state.activate_edge(&parent, &dep, domain)?;
            progress = true;
        }
        let mut retry = false;
        let mut pending = std::mem::take(&mut state.pending_weak);
        for (parent, dep_name, feature, reference, _domain) in pending.drain(..) {
            let Some((dep, dep_domain)) = state.edge(&parent, &dep_name) else {
                continue;
            };
            let dep = dep.clone();
            let active = !dep.optional
                || state
                    .active_set(dep_domain)
                    .get(&parent)
                    .is_some_and(|active| active.contains(&dep.extern_name));
            if active {
                state.enqueue_dep_feature(&parent, &dep, dep_domain, &feature, &reference)?;
                retry = true;
            } else {
                state
                    .pending_weak
                    .push((parent, dep_name, feature, reference, dep_domain));
            }
        }
        if !progress && !retry {
            break;
        }
    }

    // Weak features for deps present in the graph: cargo applies
    // `dep?/feat` whenever the dep is in the resolution (locked) graph,
    // even if no feature activated it (e.g. toml's `std` =
    // `["indexmap?/std"]` with `preserve_order` off). The edge itself is
    // not activated — the dep is not pulled into the build — but the
    // feature name lands in the dep's activated set, matching cargo's
    // resolve-node features.
    let pending = std::mem::take(&mut state.pending_weak);
    for (parent, dep_name, feature, reference, _domain) in pending {
        let Some((dep, dep_domain)) = state
            .edge(&parent, &dep_name)
            .map(|(d, dd)| (d.clone(), dd))
        else {
            continue;
        };
        let active = state
            .active_set(dep_domain)
            .get(&parent)
            .is_some_and(|active| active.contains(&dep.extern_name));
        if !active {
            state.enqueue_dep_feature(&parent, &dep, dep_domain, &feature, &reference)?;
        }
    }
    while let Some((package, feature, domain)) = state.queue.pop() {
        state.process(&package, &feature, domain)?;
    }

    Ok(FeatureMap {
        packages: state.features_on,
        active_optional_deps: state.active_optional,
        build_features: state.host_features_on,
        active_build_optional_deps: state.host_active_optional,
    })
}

/// The feature domain an activation lands in.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Domain {
    /// Normal (and dev, when included) dependencies.
    Target,
    /// Build-dependencies (resolver 3 only).
    Host,
}

/// Workqueue state of the resolution fixpoint.
struct Resolver<'a> {
    resolver: ResolverVersion,
    include_dev: bool,
    packages: &'a BTreeMap<PackageId, &'a Package>,
    features_on: BTreeMap<PackageId, BTreeSet<String>>,
    active_optional: BTreeMap<PackageId, BTreeSet<String>>,
    host_features_on: BTreeMap<PackageId, BTreeSet<String>>,
    host_active_optional: BTreeMap<PackageId, BTreeSet<String>>,
    default_on: BTreeMap<PackageId, bool>,
    host_default_on: BTreeMap<PackageId, bool>,
    queue: Vec<(PackageId, String, Domain)>,
    /// Edges whose target package must be activated (with the parent
    /// identity resolved from the queue).
    edge_queue: Vec<(PackageId, Dep, Domain)>,
    /// Weak `dep?/feat` references whose dep was not active yet; re-checked
    /// at the fixpoint until the dep activates (cargo semantics: the
    /// feature applies once the dep is enabled, regardless of ref order).
    pending_weak: Vec<(PackageId, String, String, String, Domain)>,
    /// Packages already expanded (non-optional edges activated) in a
    /// domain.
    expanded: BTreeSet<(PackageId, Domain)>,
    native_imports: &'a BTreeSet<&'a str>,
}

impl<'a> Resolver<'a> {
    fn pkg(&self, id: &PackageId) -> Result<&'a Package, FeatureError> {
        self.packages
            .get(id)
            .copied()
            .ok_or_else(|| FeatureError::UnknownPackage(id.name.clone()))
    }

    /// The domain's active-optional map.
    fn active_set(&mut self, domain: Domain) -> &mut BTreeMap<PackageId, BTreeSet<String>> {
        if domain == Domain::Host {
            &mut self.host_active_optional
        } else {
            &mut self.active_optional
        }
    }

    /// The edges of `package` in `domain`, per resolver semantics:
    ///
    /// - Resolver 1: normal + build + dev (unified).
    /// - Resolver 2: normal + build (+ dev when included).
    /// - Resolver 3, target: normal (+ dev when included); host: normal +
    ///   build (a host crate's normal deps compile for the host too).
    fn edges_for<'b>(&self, package: &'b Package, domain: Domain) -> Vec<&'b Dep> {
        let mut out: Vec<&'b Dep> = Vec::new();
        match (self.resolver, domain) {
            (ResolverVersion::V1, Domain::Target) => {
                out.extend(package.deps.iter());
                out.extend(package.build_deps.iter());
                out.extend(package.dev_deps.iter());
            }
            (ResolverVersion::V2, Domain::Target) => {
                out.extend(package.deps.iter());
                out.extend(package.build_deps.iter());
                if self.include_dev {
                    out.extend(package.dev_deps.iter());
                }
            }
            (ResolverVersion::V3, Domain::Target) => {
                out.extend(package.deps.iter());
                if self.include_dev {
                    out.extend(package.dev_deps.iter());
                }
            }
            (ResolverVersion::V3, Domain::Host) => {
                out.extend(package.deps.iter());
                out.extend(package.build_deps.iter());
            }
            (ResolverVersion::V1 | ResolverVersion::V2, Domain::Host) => {}
        }
        out
    }

    /// Finds a dependency by declared name or extern name across every
    /// edge kind, returning the edge and the domain it belongs to.
    fn edge(&self, package: &PackageId, name: &str) -> Option<(&Dep, Domain)> {
        let pkg = self.packages.get(package)?;
        // Declared names compare dash-insensitively: rustls-webpki
        // declares `pki-types` (extern `pki_types`, crate
        // `rustls-pki-types`) and its features reference `pki-types/alloc`.
        let normalized = name.replace('-', "_");
        let matches = |dep: &Dep| {
            dep.package.name == name
                || dep.extern_name == name
                || dep.extern_name.replace('-', "_") == normalized
                || dep.package.name.replace('-', "_") == normalized
        };
        if let Some(dep) = pkg.deps.iter().find(|dep| matches(dep)) {
            return Some((dep, Domain::Target));
        }
        if let Some(dep) = pkg.build_deps.iter().find(|dep| matches(dep)) {
            let domain = if self.resolver == ResolverVersion::V3 {
                Domain::Host
            } else {
                Domain::Target
            };
            return Some((dep, domain));
        }
        if self.include_dev
            && let Some(dep) = pkg.dev_deps.iter().find(|dep| matches(dep))
        {
            return Some((dep, Domain::Target));
        }
        None
    }

    /// Activates the package's non-optional edges in `domain` exactly once
    /// per (package, domain).
    fn ensure_expanded(&mut self, package: &PackageId, domain: Domain) {
        if !self.expanded.insert((package.clone(), domain)) {
            return;
        }
        let Some(pkg) = self.packages.get(package).copied() else {
            return;
        };
        for dep in self.edges_for(pkg, domain) {
            if !dep.optional && !self.native_imports.contains(dep.package.name.as_str()) {
                self.edge_queue.push((package.clone(), dep.clone(), domain));
            }
        }
    }

    fn mark_default(&mut self, package: &PackageId, domain: Domain) {
        let default_on = if domain == Domain::Host {
            &mut self.host_default_on
        } else {
            &mut self.default_on
        };
        if default_on.get(package).copied().unwrap_or(false) {
            return;
        }
        default_on.insert(package.clone(), true);
        if self
            .pkg(package)
            .is_ok_and(|package| package.has_default_feature)
        {
            self.queue
                .push((package.clone(), "default".to_owned(), domain));
        }
    }

    /// Marks a feature active; returns true when newly activated (the
    /// caller then processes its references).
    fn mark_feature(&mut self, package: &PackageId, feature: &str, domain: Domain) -> bool {
        let features = if domain == Domain::Host {
            &mut self.host_features_on
        } else {
            &mut self.features_on
        };
        features
            .entry(package.clone())
            .or_default()
            .insert(feature.to_owned())
    }

    /// Activates a dependency edge in `domain`: for optional edges, marks
    /// it active (once), then applies the edge's default-feature and
    /// feature list to the dependency package. Non-optional edges apply
    /// unconditionally.
    fn activate_edge(
        &mut self,
        parent: &PackageId,
        dep: &Dep,
        domain: Domain,
    ) -> Result<(), FeatureError> {
        if dep.optional {
            let active = self.active_set(domain).entry(parent.clone()).or_default();
            if !active.insert(dep.extern_name.clone()) {
                return Ok(());
            }
        }
        // The dependency package must exist for feature application;
        // cc_import edges are not feature-bearing (already filtered).
        if self.native_imports.contains(dep.package.name.as_str()) {
            return Ok(());
        }
        // Provisional registry edges (`tong lock` collecting mode) have a
        // placeholder identity and no package in the model yet — the edge
        // activates, but its own features are unknown until the registry
        // resolves the version.
        let provisional = matches!(
            &dep.package.source,
            crate::model::SourceId::Registry(url) if url.is_empty()
        );
        if provisional {
            return Ok(());
        }
        self.pkg(&dep.package)?;
        self.ensure_expanded(&dep.package, domain);
        if dep.default_features {
            self.mark_default(&dep.package, domain);
        }
        for feature in &dep.features {
            self.queue
                .push((dep.package.clone(), feature.clone(), domain));
        }
        Ok(())
    }

    fn process(
        &mut self,
        package: &PackageId,
        feature: &str,
        domain: Domain,
    ) -> Result<(), FeatureError> {
        let pkg = self.pkg(package)?;
        if let Some(references) = pkg.features.get(feature) {
            if self.mark_feature(package, feature, domain) {
                self.ensure_expanded(package, domain);
                for reference in references {
                    self.process_reference(package, reference, domain)?;
                }
            }
            return Ok(());
        }
        // Implicit feature: a plain name matching an optional dep activates
        // it (resolver v2 keeps the legacy `foo = ["bar"]` form working).
        // Cargo's resolve graph also lists the dep name itself among the
        // package's activated features, so the implicit activation is
        // recorded both as an edge and as a feature name.
        if let Some((dep, dep_domain)) = self.edge(package, feature) {
            let dep = dep.clone();
            if dep.optional {
                self.activate_edge(&pkg.id, &dep, dep_domain)?;
                self.mark_feature(package, &dep.package.name, domain);
                return Ok(());
            }
            return Err(FeatureError::NotOptionalDep {
                package: package.name.clone(),
                feature: feature.to_owned(),
                dep: dep.extern_name.clone(),
            });
        }
        Err(FeatureError::UnknownFeature {
            package: package.name.clone(),
            feature: feature.to_owned(),
        })
    }

    fn process_reference(
        &mut self,
        package: &PackageId,
        reference: &str,
        _domain: Domain,
    ) -> Result<(), FeatureError> {
        let pkg = self.pkg(package)?;
        // `dep:x` — namespaced activation of an optional dependency.
        if let Some(dep_name) = reference.strip_prefix("dep:") {
            let (dep, dep_domain) =
                self.edge(package, dep_name)
                    .ok_or_else(|| FeatureError::UnknownDep {
                        package: package.name.clone(),
                        dep: dep_name.to_owned(),
                    })?;
            let dep = dep.clone();
            if !dep.optional {
                // Cargo validates `dep:x` against the union of target
                // tables: `x` optional in any table makes the reference
                // legal, and on hosts where the merged edge is plain the
                // dependency is already active (wgpu's `wgpu-hal` is
                // optional only on wasm).
                if !pkg.optional_anywhere.contains(dep_name) {
                    return Err(FeatureError::NotOptionalDep {
                        package: package.name.clone(),
                        feature: reference.to_owned(),
                        dep: dep.extern_name.clone(),
                    });
                }
                return Ok(());
            }
            return self.activate_edge(&pkg.id, &dep, dep_domain);
        }

        // Plain name: a declared feature or an implicit optional dep
        // (process() decides).
        if !reference.contains('/') {
            return self.process(package, reference, Domain::Target);
        }

        let (dep_name, rest) = reference.split_once('/').unwrap_or((reference, ""));
        // `dep?/feat`: the `?` trails the dep name.
        let (dep_name, weak) = dep_name
            .strip_suffix('?')
            .map(|name| (name, true))
            .unwrap_or((dep_name, false));
        let feature = rest;
        let (dep, dep_domain) =
            self.edge(package, dep_name)
                .ok_or_else(|| FeatureError::UnknownDep {
                    package: package.name.clone(),
                    dep: dep_name.to_owned(),
                })?;
        let dep = dep.clone();

        let dep_active = if dep.optional {
            self.active_set(dep_domain)
                .get(package)
                .is_some_and(|active| active.contains(&dep.extern_name))
        } else {
            true
        };
        let _ = dep_active;
        if weak {
            // `dep?/feat` — cargo activates the dependency and applies
            // the feature like a strong reference, but the dep's own name
            // is NOT listed in the parent's node features (syn's
            // `quote?/proc-macro` lists no `quote`).
            self.activate_edge(&pkg.id, &dep, dep_domain)?;
            return self.enqueue_dep_feature(package, &dep, dep_domain, feature, reference);
        }
        // `dep/feat` — strong reference: activates the dep and its
        // feature. Cargo's resolve-node features also list the dep's own
        // name (tokio's `net = ["mio/os-poll", ...]` lists `mio`).
        self.activate_edge(&pkg.id, &dep, dep_domain)?;
        if dep.optional {
            self.mark_feature(package, &dep.package.name, _domain);
        }
        self.enqueue_dep_feature(package, &dep, dep_domain, feature, reference)
    }

    fn enqueue_dep_feature(
        &mut self,
        _parent: &PackageId,
        dep: &Dep,
        domain: Domain,
        feature: &str,
        reference: &str,
    ) -> Result<(), FeatureError> {
        // Provisional registry edges (`tong lock` collecting mode) have no
        // package in the model yet; the reference resolves after locking.
        let provisional = matches!(
            &dep.package.source,
            crate::model::SourceId::Registry(url) if url.is_empty()
        );
        if provisional {
            return Ok(());
        }
        let dep_package = self.pkg(&dep.package)?;
        // The feature may be the implicit feature of an optional dep
        // (e.g. `tracing/log` — tracing's `log` dep has no explicit
        // feature entry; cargo resolves the reference to the dep).
        let implicit = dep_package.deps.iter().any(|dep| {
            // Dash-insensitive: mongodb's `mongocrypt/bson-2`
            // references the optional renamed dep `bson-2` whose
            // extern name is `bson_2`.
            dep.optional
                && (dep.package.name == feature
                    || dep.extern_name == feature
                    || dep.extern_name.replace('-', "_") == feature.replace('-', "_")
                    || dep.package.name.replace('-', "_") == feature.replace('-', "_"))
        });
        if !dep_package.features.contains_key(feature) && !implicit {
            return Err(FeatureError::UnknownDepFeature {
                package: dep_package.name.clone(),
                feature: reference.to_owned(),
                dep: dep.extern_name.clone(),
            });
        }
        self.queue
            .push((dep.package.clone(), feature.to_owned(), domain));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SourceId;

    fn pid(name: &str) -> PackageId {
        PackageId {
            name: name.to_owned(),
            version: semver::Version::new(0, 1, 0),
            source: SourceId::Workspace(".".to_owned()),
        }
    }

    fn package(name: &str, features: &[(&str, &[&str])], has_default: bool) -> Package {
        Package {
            id: pid(name),
            name: name.to_owned(),
            dir: std::path::PathBuf::from(name),
            version: "0.1.0".to_owned(),
            edition: crate::model::Edition::E2021,
            lib: None,
            bins: Vec::new(),
            examples: Vec::new(),
            tests: Vec::new(),
            build_script: None,
            links: None,
            deps: Vec::new(),
            optional_anywhere: BTreeSet::new(),
            build_deps: Vec::new(),
            dev_deps: Vec::new(),
            features: features
                .iter()
                .map(|(name, refs)| {
                    (
                        (*name).to_owned(),
                        refs.iter().map(|r| (*r).to_owned()).collect(),
                    )
                })
                .collect(),
            has_default_feature: has_default,
            rustflags: Vec::new(),
            env: BTreeMap::new(),
        }
    }

    fn dep(extern_name: &str, package: &str, optional: bool) -> Dep {
        Dep {
            extern_name: extern_name.to_owned(),
            package: pid(package),
            optional,
            default_features: true,
            features: Vec::new(),
            target: None,
        }
    }

    fn model(packages: Vec<Package>, members: &[&str]) -> RustModel {
        RustModel {
            packages,
            members: members.iter().map(|m| pid(m)).collect(),
            ..Default::default()
        }
    }

    fn request(package: &str, features: &[&str]) -> FeatureRequest {
        FeatureRequest {
            package: pid(package),
            features: features.iter().map(|f| (*f).to_owned()).collect(),
            default_features: true,
        }
    }

    #[test]
    fn activates_default_and_explicit_features() {
        let mut app = package("app", &[("default", &["std"]), ("std", &[])], true);
        app.deps.push(dep("core", "core", false));
        let core = package("core", &[("default", &[]), ("alloc", &[])], true);
        let model = model(vec![app, core], &["app"]);
        let map = resolve_features(&model, &[request("app", &["std"])], false).unwrap();
        let app_features = &map.packages[&pid("app")];
        assert!(app_features.contains("std"));
        assert!(app_features.contains("default"));
        assert!(map.packages[&pid("core")].contains("default"));
    }

    #[test]
    fn no_default_feature_when_not_declared() {
        let app = package("app", &[], false);
        let model = model(vec![app], &["app"]);
        let map = resolve_features(&model, &[request("app", &[])], false).unwrap();
        assert!(map.packages[&pid("app")].is_empty());
    }

    #[test]
    fn optional_dep_activates_via_dep_namespace() {
        let mut app = package("app", &[("default", &["dep:extra"]), ("extra", &[])], true);
        app.deps.push(dep("extra", "extra", true));
        let extra = package("extra", &[("default", &[])], true);
        let model = model(vec![app, extra], &["app"]);
        let map = resolve_features(&model, &[request("app", &[])], false).unwrap();
        // `default` activates `dep:extra` — the dep, not the `extra` feature.
        assert_eq!(map.packages[&pid("app")].len(), 1);
        assert!(map.packages[&pid("extra")].contains("default"));
        assert!(map.active_optional_deps[&pid("app")].contains("extra"));
    }

    #[test]
    fn inactive_optional_dep_is_not_extern() {
        let mut app = package("app", &[("default", &[])], true);
        app.deps.push(dep("extra", "extra", true));
        let extra = package("extra", &[("default", &[])], true);
        let model = model(vec![app, extra], &["app"]);
        let map = resolve_features(&model, &[request("app", &[])], false).unwrap();
        assert!(!map.active_optional_deps.contains_key(&pid("app")));
        assert!(map.packages[&pid("extra")].is_empty());
    }

    #[test]
    fn weak_dep_feature_requires_active_dep() {
        // `extra?/feat` with extra inactive: ignored, no error. The weak
        // reference is evaluated when its containing feature is processed,
        // so `dep:extra` must come first in the list (Cargo v2 behavior).
        let mut app = package(
            "app",
            &[
                ("default", &["dep:extra", "extra?/feat"]),
                ("f", &["extra?/feat"]),
            ],
            true,
        );
        app.deps.push(dep("extra", "extra", true));
        let mut extra = package("extra", &[("default", &[]), ("feat", &[])], true);
        extra.deps.push(dep("dep", "dep", false));
        let dep = package("dep", &[], false);
        let model = model(vec![app, extra, dep], &["app"]);
        let map = resolve_features(&model, &[request("app", &[])], false).unwrap();
        // `dep:extra` activates extra; the following weak reference in the
        // same list then applies `feat`.
        assert!(map.packages[&pid("extra")].contains("feat"));
        let map = resolve_features(&model, &[request("app", &["f"])], false).unwrap();
        assert!(map.packages[&pid("extra")].contains("feat"));
    }

    /// Cargo activates optional deps referenced by `dep?/feat` weak
    /// references exactly like strong ones (verified against `cargo
    /// metadata`: `futures-core?/alloc` in a default feature activates
    /// the dep, its edge, and the feature).
    #[test]
    fn weak_dep_feature_activates_dep() {
        let mut app = package("app", &[("default", &["extra?/feat"])], true);
        app.deps.push(dep("extra", "extra", true));
        let extra = package("extra", &[("default", &[]), ("feat", &[])], true);
        let model = model(vec![app, extra], &["app"]);
        let map = resolve_features(&model, &[request("app", &[])], false).unwrap();
        assert!(map.active_optional_deps[&pid("app")].contains("extra"));
        assert!(map.packages[&pid("extra")].contains("feat"));
    }

    #[test]
    fn strong_dep_feature_activates_dep() {
        let mut app = package("app", &[("default", &["extra/feat"])], true);
        app.deps.push(dep("extra", "extra", true));
        let extra = package("extra", &[("default", &[]), ("feat", &[])], true);
        let model = model(vec![app, extra], &["app"]);
        let map = resolve_features(&model, &[request("app", &[])], false).unwrap();
        assert!(map.active_optional_deps[&pid("app")].contains("extra"));
        assert!(map.packages[&pid("extra")].contains("feat"));
    }

    #[test]
    fn edge_features_propagate_to_dep() {
        let mut app = package("app", &[("default", &["dep:extra"])], true);
        app.deps.push(Dep {
            extern_name: "extra".to_owned(),
            package: pid("extra"),
            optional: true,
            default_features: false,
            features: vec!["feat".to_owned()],
            target: None,
        });
        let extra = package("extra", &[("default", &[]), ("feat", &[])], true);
        let model = model(vec![app, extra], &["app"]);
        let map = resolve_features(&model, &[request("app", &[])], false).unwrap();
        // Edge default-features=false: no default on extra.
        assert!(!map.packages[&pid("extra")].contains("default"));
        assert!(map.packages[&pid("extra")].contains("feat"));
    }

    #[test]
    fn unknown_feature_is_an_error() {
        let app = package("app", &[("default", &["nope"])], true);
        let model = model(vec![app], &["app"]);
        let err = resolve_features(&model, &[request("app", &[])], false).unwrap_err();
        assert_eq!(
            err,
            FeatureError::UnknownFeature {
                package: "app".to_owned(),
                feature: "nope".to_owned(),
            }
        );
    }

    #[test]
    fn unknown_dep_feature_is_an_error() {
        let mut app = package("app", &[("default", &["extra/missing"])], true);
        app.deps.push(dep("extra", "extra", false));
        let extra = package("extra", &[("default", &[])], true);
        let model = model(vec![app, extra], &["app"]);
        let err = resolve_features(&model, &[request("app", &[])], false).unwrap_err();
        assert!(
            matches!(err, FeatureError::UnknownDepFeature { .. }),
            "{err}"
        );
    }

    #[test]
    fn implicit_feature_activates_optional_dep() {
        let mut app = package("app", &[("default", &["extra"])], true);
        app.deps.push(dep("extra", "extra", true));
        let extra = package("extra", &[("default", &[])], true);
        let model = model(vec![app, extra], &["app"]);
        let map = resolve_features(&model, &[request("app", &[])], false).unwrap();
        assert!(map.active_optional_deps[&pid("app")].contains("extra"));
    }

    #[test]
    fn feature_unification_across_edges() {
        // Two packages both depend on shared; one wants feat, one doesn't.
        // Union semantics: feat is on.
        let mut app = package("app", &[("default", &["shared/feat"])], true);
        app.deps.push(dep("shared", "shared", false));
        let mut other = package("other", &[("default", &[])], true);
        other.deps.push(dep("shared", "shared", false));
        let shared = package("shared", &[("default", &[]), ("feat", &[])], true);
        let model = model(vec![app, other, shared], &["app", "other"]);
        let map =
            resolve_features(&model, &[request("app", &[]), request("other", &[])], false).unwrap();
        assert!(map.packages[&pid("shared")].contains("feat"));
    }

    #[test]
    fn dev_deps_only_apply_when_included() {
        let mut app = package("app", &[("default", &[])], true);
        app.dev_deps.push(Dep {
            extern_name: "devdep".to_owned(),
            package: pid("devdep"),
            optional: false,
            default_features: true,
            features: vec!["devfeat".to_owned()],
            target: None,
        });
        let devdep = package("devdep", &[("default", &[]), ("devfeat", &[])], true);
        let model = model(vec![app, devdep], &["app"]);
        // Normal build: dev-dep edges ignored.
        let map = resolve_features(&model, &[request("app", &[])], false).unwrap();
        assert!(map.packages[&pid("devdep")].is_empty());
        // Test build: dev-dep edges apply.
        let map = resolve_features(&model, &[request("app", &[])], true).unwrap();
        assert!(map.packages[&pid("devdep")].contains("devfeat"));
    }

    #[test]
    fn feature_map_canonical_encoding_is_deterministic() {
        let mut app = package("app", &[("default", &[]), ("x", &[])], true);
        app.deps.push(dep("extra", "extra", true));
        let extra = package("extra", &[("default", &[])], true);
        let model = model(vec![app, extra], &["app"]);
        let map = resolve_features(&model, &[request("app", &["x"])], false).unwrap();
        let bytes = tong_core::canonical::encode_vec(&map);
        let again = resolve_features(&model, &[request("app", &["x"])], false).unwrap();
        assert_eq!(bytes, tong_core::canonical::encode_vec(&again));
    }

    /// Resolver 1 unifies dev-dep features into the whole graph, even for
    /// a normal build (the legacy behavior).
    #[test]
    fn resolver_v1_unifies_dev_features_in_normal_builds() {
        let mut app = package("app", &[("default", &["shared/feat"])], true);
        app.deps.push(dep("shared", "shared", false));
        app.dev_deps.push(Dep {
            extern_name: "shared".to_owned(),
            package: pid("shared"),
            optional: false,
            default_features: true,
            features: vec!["devfeat".to_owned()],
            target: None,
        });
        let shared = package(
            "shared",
            &[("default", &[]), ("feat", &[]), ("devfeat", &[])],
            true,
        );
        let mut model = model(vec![app, shared], &["app"]);
        model.resolver = ResolverVersion::V1;
        // Normal build (include_dev_deps = false): the dev edge still
        // unifies `devfeat` under resolver 1.
        let map = resolve_features(&model, &[request("app", &[])], false).unwrap();
        assert!(map.packages[&pid("shared")].contains("devfeat"));

        // Resolver 2 does not: dev features stay out of normal builds.
        model.resolver = ResolverVersion::V2;
        let map = resolve_features(&model, &[request("app", &[])], false).unwrap();
        assert!(!map.packages[&pid("shared")].contains("devfeat"));
        let map = resolve_features(&model, &[request("app", &[])], true).unwrap();
        assert!(map.packages[&pid("shared")].contains("devfeat"));
    }

    /// Resolver 3 keeps host (build-dependency) and target (normal
    /// dependency) feature sets of one package independent; resolver 2
    /// unifies them.
    #[test]
    fn resolver_v3_separates_host_and_target_domains() {
        let mut app = package("app", &[("default", &[])], true);
        app.build_script = Some(std::path::PathBuf::from("build.rs"));
        app.deps.push(Dep {
            extern_name: "shared".to_owned(),
            package: pid("shared"),
            optional: false,
            default_features: false,
            features: Vec::new(),
            target: None,
        });
        app.build_deps.push(Dep {
            extern_name: "shared".to_owned(),
            package: pid("shared"),
            optional: false,
            default_features: false,
            features: vec!["hostfeat".to_owned()],
            target: None,
        });
        let shared = package("shared", &[("default", &[]), ("hostfeat", &[])], true);
        let mut model = model(vec![app, shared], &["app"]);
        model.resolver = ResolverVersion::V3;
        let map = resolve_features(&model, &[request("app", &[])], false).unwrap();
        // Target domain: the normal edge requests nothing.
        assert!(map.packages[&pid("shared")].is_empty());
        // Host domain: the build edge's feature applies there.
        assert!(map.build_features[&pid("shared")].contains("hostfeat"));

        // Resolver 2: build edges share the target domain — the feature
        // unifies into `packages`.
        model.resolver = ResolverVersion::V2;
        let map = resolve_features(&model, &[request("app", &[])], false).unwrap();
        assert!(map.packages[&pid("shared")].contains("hostfeat"));
        assert!(map.build_features[&pid("shared")].is_empty());
    }

    /// Resolver 3: a host package's normal dependencies activate in the
    /// host domain.
    #[test]
    fn resolver_v3_host_subgraph_includes_normal_deps() {
        let mut app = package("app", &[("default", &[])], true);
        app.build_script = Some(std::path::PathBuf::from("build.rs"));
        app.build_deps.push(dep("hostpkg", "hostpkg", false));
        let mut hostpkg = package("hostpkg", &[("default", &[])], true);
        hostpkg.deps.push(dep("leaf", "leaf", false));
        let leaf = package("leaf", &[("default", &[])], true);
        let mut model = model(vec![app, hostpkg, leaf], &["app"]);
        model.resolver = ResolverVersion::V3;
        let map = resolve_features(&model, &[request("app", &[])], false).unwrap();
        assert!(map.build_features[&pid("leaf")].contains("default"));
        // The target domain never reached leaf (no normal edge).
        assert!(map.packages[&pid("leaf")].is_empty());
    }

    /// Two versions of one crate keep fully distinct feature sets.
    #[test]
    fn two_versions_have_distinct_feature_sets() {
        let mut app = package("app", &[("default", &["alpha1/feat"])], true);
        app.deps.push(Dep {
            extern_name: "alpha1".to_owned(),
            package: PackageId {
                name: "alpha".to_owned(),
                version: semver::Version::new(1, 0, 0),
                source: SourceId::Registry("fixture".to_owned()),
            },
            optional: false,
            default_features: true,
            features: Vec::new(),
            target: None,
        });
        app.deps.push(Dep {
            extern_name: "alpha2".to_owned(),
            package: PackageId {
                name: "alpha".to_owned(),
                version: semver::Version::new(2, 0, 0),
                source: SourceId::Registry("fixture".to_owned()),
            },
            optional: false,
            default_features: false,
            features: Vec::new(),
            target: None,
        });
        let alpha1 = Package {
            id: PackageId {
                name: "alpha".to_owned(),
                version: semver::Version::new(1, 0, 0),
                source: SourceId::Registry("fixture".to_owned()),
            },
            name: "alpha".to_owned(),
            dir: std::path::PathBuf::from("alpha-1"),
            version: "1.0.0".to_owned(),
            edition: crate::model::Edition::E2021,
            lib: None,
            bins: Vec::new(),
            examples: Vec::new(),
            tests: Vec::new(),
            build_script: None,
            links: None,
            deps: Vec::new(),
            optional_anywhere: BTreeSet::new(),
            build_deps: Vec::new(),
            dev_deps: Vec::new(),
            features: BTreeMap::from([
                ("default".to_owned(), Vec::new()),
                ("feat".to_owned(), Vec::new()),
            ]),
            has_default_feature: true,
            rustflags: Vec::new(),
            env: BTreeMap::new(),
        };
        let alpha2 = Package {
            id: PackageId {
                name: "alpha".to_owned(),
                version: semver::Version::new(2, 0, 0),
                source: SourceId::Registry("fixture".to_owned()),
            },
            name: "alpha".to_owned(),
            dir: std::path::PathBuf::from("alpha-2"),
            version: "2.0.0".to_owned(),
            edition: crate::model::Edition::E2021,
            lib: None,
            bins: Vec::new(),
            examples: Vec::new(),
            tests: Vec::new(),
            build_script: None,
            links: None,
            deps: Vec::new(),
            optional_anywhere: BTreeSet::new(),
            build_deps: Vec::new(),
            dev_deps: Vec::new(),
            features: BTreeMap::from([("default".to_owned(), Vec::new())]),
            has_default_feature: true,
            rustflags: Vec::new(),
            env: BTreeMap::new(),
        };
        let model = model(vec![app, alpha1, alpha2], &["app"]);
        let map = resolve_features(&model, &[request("app", &[])], false).unwrap();
        let id1 = PackageId {
            name: "alpha".to_owned(),
            version: semver::Version::new(1, 0, 0),
            source: SourceId::Registry("fixture".to_owned()),
        };
        let id2 = PackageId {
            name: "alpha".to_owned(),
            version: semver::Version::new(2, 0, 0),
            source: SourceId::Registry("fixture".to_owned()),
        };
        assert!(map.packages[&id1].contains("default"));
        assert!(map.packages[&id1].contains("feat"));
        // Version 2 has no `feat` and its default was disabled by the edge.
        assert!(!map.packages[&id2].contains("feat"));
        assert!(!map.packages[&id2].contains("default"));
    }
}
