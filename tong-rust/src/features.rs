//! Cargo-compatible feature resolution (resolver v2 semantics).
//!
//! Pure module, no I/O; deterministic (BTreeMap/BTreeSet everywhere).
//! Implements resolver-v2 rules: weak dep features (`pkg?/feat`), namespaced
//! `dep:` features, optional deps → implicit `dep_name` features, the
//! `default` feature, `default-features = false`, and dependency feature
//! propagation via `dep/feat` with Cargo's union (unification) semantics.
//!
//! The resolution is a fixpoint workqueue: activating a feature enqueues
//! the references it declares; every (package, feature) pair is processed
//! at most once, so the algorithm always terminates.

use std::collections::{BTreeMap, BTreeSet};

use tong_core::canonical::{CanonicalEncode, Encoder};

use crate::model::{Dep, Package, RustModel};

/// A feature request for a workspace package (CLI `--features`,
/// `--no-default-features`, `--all-features`, or `Tong.toml` target
/// `features`).
#[derive(Clone, Debug, Default)]
pub struct FeatureRequest {
    /// Package name.
    pub package: String,
    /// Features to activate.
    pub features: Vec<String>,
    /// Whether the package's default feature is enabled.
    pub default_features: bool,
}

/// The resolved feature activation state of the whole graph.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FeatureMap {
    /// Activated features per package (sorted).
    pub packages: BTreeMap<String, BTreeSet<String>>,
    /// Active optional dep edges per package: extern name of the dep.
    /// Needed because an activated optional dep may carry no features of
    /// its own (e.g. `default-features = false` with an empty list), in
    /// which case the package's feature set is empty but the edge is live.
    pub active_optional_deps: BTreeMap<String, BTreeSet<String>>,
}

impl CanonicalEncode for FeatureMap {
    fn encode(&self, enc: &mut Encoder) {
        enc.write_u64(self.packages.len() as u64);
        for (package, features) in &self.packages {
            enc.write_str(package);
            enc.write_seq(&features.iter().cloned().collect::<Vec<_>>());
        }
        enc.write_u64(self.active_optional_deps.len() as u64);
        for (package, deps) in &self.active_optional_deps {
            enc.write_str(package);
            enc.write_seq(&deps.iter().cloned().collect::<Vec<_>>());
        }
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

/// Resolves the feature activation state of the whole model.
///
/// `requests` seed the workspace members (default: all members with
/// `default_features: true`). Non-optional dep edges of every package are
/// always active; optional edges activate only through features.
/// `include_dev_deps` additionally treats dev-dep edges as always active
/// (test builds; dev-deps are excluded from normal builds, like Cargo).
pub fn resolve_features(
    model: &RustModel,
    requests: &[FeatureRequest],
    include_dev_deps: bool,
) -> Result<FeatureMap, FeatureError> {
    let packages: BTreeMap<&str, &Package> = model
        .packages
        .iter()
        .map(|package| (package.name.as_str(), package))
        .collect();

    let mut state = Resolver {
        packages: &packages,
        features_on: packages
            .keys()
            .map(|name| ((*name).to_owned(), BTreeSet::new()))
            .collect(),
        default_on: BTreeMap::new(),
        active_optional: BTreeMap::new(),
        queue: Vec::new(),
    };

    // Seed: explicit workspace-member requests.
    for request in requests {
        let package = state.pkg(&request.package)?;
        if request.default_features {
            state.mark_default(&package.name);
        }
        for feature in &request.features {
            state.queue.push((package.name.clone(), feature.clone()));
        }
    }

    // Seed: non-optional edges of every package are always active (their
    // feature lists and default-features apply unconditionally).
    for package in &model.packages {
        for dep in state.edges(package, include_dev_deps) {
            if !dep.optional {
                state.activate_edge(package, dep, include_dev_deps)?;
            }
        }
    }

    // Fixpoint: process queued (package, feature) activations.
    while let Some((package, feature)) = state.queue.pop() {
        state.process(&package, &feature, include_dev_deps)?;
    }

    Ok(FeatureMap {
        packages: state.features_on,
        active_optional_deps: state.active_optional,
    })
}

/// Workqueue state of the resolution fixpoint.
struct Resolver<'a> {
    packages: &'a BTreeMap<&'a str, &'a Package>,
    features_on: BTreeMap<String, BTreeSet<String>>,
    default_on: BTreeMap<String, bool>,
    active_optional: BTreeMap<String, BTreeSet<String>>,
    queue: Vec<(String, String)>,
}

impl<'a> Resolver<'a> {
    fn pkg(&self, name: &str) -> Result<&'a Package, FeatureError> {
        self.packages
            .get(name)
            .copied()
            .ok_or_else(|| FeatureError::UnknownPackage(name.to_owned()))
    }

    fn edges<'b>(&self, package: &'b Package, include_dev: bool) -> Vec<&'b Dep> {
        package
            .deps
            .iter()
            .chain(package.build_deps.iter())
            .chain(if include_dev {
                package.dev_deps.iter()
            } else {
                [].iter()
            })
            .collect()
    }

    fn edge<'b>(&self, package: &'b Package, name: &str, include_dev: bool) -> Option<&'b Dep> {
        self.edges(package, include_dev)
            .into_iter()
            .find(|dep| dep.extern_name == name)
    }

    /// Requests the package's default feature (idempotent); only enqueues
    /// when the package declares one.
    fn mark_default(&mut self, package: &str) {
        if self.default_on.get(package).copied().unwrap_or(false) {
            return;
        }
        self.default_on.insert(package.to_owned(), true);
        if self
            .pkg(package)
            .is_ok_and(|package| package.has_default_feature)
        {
            self.queue.push((package.to_owned(), "default".to_owned()));
        }
    }

    /// Marks a feature active; returns true when newly activated (the
    /// caller then processes its references).
    fn mark_feature(&mut self, package: &str, feature: &str) -> bool {
        self.features_on
            .entry(package.to_owned())
            .or_default()
            .insert(feature.to_owned())
    }

    /// Activates a dependency edge: for optional edges, marks it active
    /// (once), then applies the edge's default-feature and feature list to
    /// the dependency package. Non-optional edges apply unconditionally.
    fn activate_edge(
        &mut self,
        parent: &Package,
        dep: &Dep,
        _include_dev: bool,
    ) -> Result<(), FeatureError> {
        if dep.optional {
            let active = self.active_optional.entry(parent.name.clone()).or_default();
            if !active.insert(dep.extern_name.clone()) {
                return Ok(());
            }
        }
        // The dependency package must exist for feature application.
        self.pkg(&dep.package)?;
        if dep.default_features {
            self.mark_default(&dep.package);
        }
        for feature in &dep.features {
            self.queue.push((dep.package.clone(), feature.clone()));
        }
        Ok(())
    }

    fn process(
        &mut self,
        package_name: &str,
        feature: &str,
        include_dev: bool,
    ) -> Result<(), FeatureError> {
        let package = self.pkg(package_name)?;
        if let Some(references) = package.features.get(feature) {
            if self.mark_feature(package_name, feature) {
                for reference in references {
                    self.process_reference(package, reference, include_dev)?;
                }
            }
            return Ok(());
        }
        // Implicit feature: a plain name matching an optional dep activates
        // it (resolver v2 keeps the legacy `foo = ["bar"]` form working).
        if let Some(dep) = self.edge(package, feature, include_dev) {
            if dep.optional {
                self.activate_edge(package, dep, include_dev)?;
                return Ok(());
            }
            return Err(FeatureError::NotOptionalDep {
                package: package_name.to_owned(),
                feature: feature.to_owned(),
                dep: dep.extern_name.clone(),
            });
        }
        Err(FeatureError::UnknownFeature {
            package: package_name.to_owned(),
            feature: feature.to_owned(),
        })
    }

    fn process_reference(
        &mut self,
        package: &Package,
        reference: &str,
        include_dev: bool,
    ) -> Result<(), FeatureError> {
        // `dep:x` — namespaced activation of an optional dependency.
        if let Some(dep_name) = reference.strip_prefix("dep:") {
            let dep = self.edge(package, dep_name, include_dev).ok_or_else(|| {
                FeatureError::UnknownDep {
                    package: package.name.clone(),
                    dep: dep_name.to_owned(),
                }
            })?;
            if !dep.optional {
                return Err(FeatureError::NotOptionalDep {
                    package: package.name.clone(),
                    feature: reference.to_owned(),
                    dep: dep.extern_name.clone(),
                });
            }
            return self.activate_edge(package, dep, include_dev);
        }

        // Plain name: a declared feature or an implicit optional dep
        // (process() decides).
        if !reference.contains('/') {
            return self.process(&package.name, reference, include_dev);
        }

        let (dep_name, rest) = reference.split_once('/').unwrap_or((reference, ""));
        // `dep?/feat`: the `?` trails the dep name.
        let (dep_name, weak) = dep_name
            .strip_suffix('?')
            .map(|name| (name, true))
            .unwrap_or((dep_name, false));
        let feature = rest;
        let dep =
            self.edge(package, dep_name, include_dev)
                .ok_or_else(|| FeatureError::UnknownDep {
                    package: package.name.clone(),
                    dep: dep_name.to_owned(),
                })?;

        let dep_active = if dep.optional {
            self.active_optional
                .get(&package.name)
                .is_some_and(|active| active.contains(&dep.extern_name))
        } else {
            true
        };
        if weak {
            // `dep?/feat` — activate the dep feature only when the dep is
            // already active.
            if dep_active {
                self.enqueue_dep_feature(package, dep, feature, reference)?;
            }
            return Ok(());
        }
        // `dep/feat` — strong reference: activates the dep and its
        // feature.
        self.activate_edge(package, dep, include_dev)?;
        self.enqueue_dep_feature(package, dep, feature, reference)
    }

    fn enqueue_dep_feature(
        &mut self,
        _parent: &Package,
        dep: &Dep,
        feature: &str,
        reference: &str,
    ) -> Result<(), FeatureError> {
        let dep_package = self.pkg(&dep.package)?;
        if !dep_package.features.contains_key(feature) {
            return Err(FeatureError::UnknownDepFeature {
                package: dep_package.name.clone(),
                feature: reference.to_owned(),
                dep: dep.extern_name.clone(),
            });
        }
        self.queue.push((dep.package.clone(), feature.to_owned()));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package(name: &str, features: &[(&str, &[&str])], has_default: bool) -> Package {
        Package {
            name: name.to_owned(),
            dir: std::path::PathBuf::from(name),
            version: "0.1.0".to_owned(),
            edition: crate::model::Edition::E2021,
            lib: None,
            bins: Vec::new(),
            build_script: None,
            deps: Vec::new(),
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
            package: package.to_owned(),
            optional,
            default_features: true,
            features: Vec::new(),
            target: None,
        }
    }

    fn model(packages: Vec<Package>, members: &[&str]) -> RustModel {
        RustModel {
            packages,
            members: members.iter().map(|m| (*m).to_owned()).collect(),
            ..Default::default()
        }
    }

    fn request(package: &str, features: &[&str]) -> FeatureRequest {
        FeatureRequest {
            package: package.to_owned(),
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
        let app_features = &map.packages["app"];
        assert!(app_features.contains("std"));
        assert!(app_features.contains("default"));
        assert!(map.packages["core"].contains("default"));
    }

    #[test]
    fn no_default_feature_when_not_declared() {
        let app = package("app", &[], false);
        let model = model(vec![app], &["app"]);
        let map = resolve_features(&model, &[request("app", &[])], false).unwrap();
        assert!(map.packages["app"].is_empty());
    }

    #[test]
    fn optional_dep_activates_via_dep_namespace() {
        let mut app = package("app", &[("default", &["dep:extra"]), ("extra", &[])], true);
        app.deps.push(dep("extra", "extra", true));
        let extra = package("extra", &[("default", &[])], true);
        let model = model(vec![app, extra], &["app"]);
        let map = resolve_features(&model, &[request("app", &[])], false).unwrap();
        // `default` activates `dep:extra` — the dep, not the `extra` feature.
        assert_eq!(map.packages["app"].len(), 1);
        assert!(map.packages["extra"].contains("default"));
        assert!(map.active_optional_deps["app"].contains("extra"));
    }

    #[test]
    fn inactive_optional_dep_is_not_extern() {
        let mut app = package("app", &[("default", &[])], true);
        app.deps.push(dep("extra", "extra", true));
        let extra = package("extra", &[("default", &[])], true);
        let model = model(vec![app, extra], &["app"]);
        let map = resolve_features(&model, &[request("app", &[])], false).unwrap();
        assert!(!map.active_optional_deps.contains_key("app"));
        assert!(map.packages["extra"].is_empty());
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
        assert!(map.packages["extra"].contains("feat"));
        // A weak reference evaluated before activation stays off: `f` is
        // not requested here, but if it were, extra would already be
        // active at that point.
        let map = resolve_features(&model, &[request("app", &["f"])], false).unwrap();
        assert!(map.packages["extra"].contains("feat"));
    }

    #[test]
    fn weak_dep_feature_skipped_when_inactive() {
        let mut app = package("app", &[("default", &["extra?/feat"])], true);
        app.deps.push(dep("extra", "extra", true));
        let extra = package("extra", &[("default", &[]), ("feat", &[])], true);
        let model = model(vec![app, extra], &["app"]);
        let map = resolve_features(&model, &[request("app", &[])], false).unwrap();
        assert!(!map.active_optional_deps.contains_key("app"));
        assert!(map.packages["extra"].is_empty());
    }

    #[test]
    fn strong_dep_feature_activates_dep() {
        let mut app = package("app", &[("default", &["extra/feat"])], true);
        app.deps.push(dep("extra", "extra", true));
        let extra = package("extra", &[("default", &[]), ("feat", &[])], true);
        let model = model(vec![app, extra], &["app"]);
        let map = resolve_features(&model, &[request("app", &[])], false).unwrap();
        assert!(map.active_optional_deps["app"].contains("extra"));
        assert!(map.packages["extra"].contains("feat"));
    }

    #[test]
    fn edge_features_propagate_to_dep() {
        let mut app = package("app", &[("default", &["dep:extra"])], true);
        app.deps.push(Dep {
            extern_name: "extra".to_owned(),
            package: "extra".to_owned(),
            optional: true,
            default_features: false,
            features: vec!["feat".to_owned()],
            target: None,
        });
        let extra = package("extra", &[("default", &[]), ("feat", &[])], true);
        let model = model(vec![app, extra], &["app"]);
        let map = resolve_features(&model, &[request("app", &[])], false).unwrap();
        // Edge default-features=false: no default on extra.
        assert!(!map.packages["extra"].contains("default"));
        assert!(map.packages["extra"].contains("feat"));
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
        assert!(map.active_optional_deps["app"].contains("extra"));
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
        assert!(map.packages["shared"].contains("feat"));
    }

    #[test]
    fn dev_deps_only_apply_when_included() {
        let mut app = package("app", &[("default", &[])], true);
        app.dev_deps.push(Dep {
            extern_name: "devdep".to_owned(),
            package: "devdep".to_owned(),
            optional: false,
            default_features: true,
            features: vec!["devfeat".to_owned()],
            target: None,
        });
        let devdep = package("devdep", &[("default", &[]), ("devfeat", &[])], true);
        let model = model(vec![app, devdep], &["app"]);
        // Normal build: dev-dep edges ignored.
        let map = resolve_features(&model, &[request("app", &[])], false).unwrap();
        assert!(map.packages["devdep"].is_empty());
        // Test build: dev-dep edges apply.
        let map = resolve_features(&model, &[request("app", &[])], true).unwrap();
        assert!(map.packages["devdep"].contains("devfeat"));
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
}
