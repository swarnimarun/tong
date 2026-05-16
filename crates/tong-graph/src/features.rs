use std::collections::{BTreeMap, BTreeSet};
use tong_manifest::{Dependency, Manifest};

#[derive(Debug, Clone)]
pub(super) struct FeatureResolution {
    pub(super) enabled_features: BTreeSet<String>,
    pub(super) optional_dependencies: BTreeSet<String>,
    pub(super) dependency_features: BTreeMap<String, BTreeSet<String>>,
}

pub(super) fn resolve_features(
    manifest: &Manifest,
    requested_features: BTreeSet<String>,
    default_features: bool,
) -> FeatureResolution {
    let mut enabled_features = BTreeSet::new();
    let mut optional_dependencies = BTreeSet::new();
    let mut dependency_features: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut stack = requested_features.into_iter().collect::<Vec<_>>();
    if default_features && manifest.features.contains_key("default") {
        stack.push("default".to_owned());
    }

    while let Some(feature) = stack.pop() {
        if !enabled_features.insert(feature.clone()) {
            continue;
        }

        let Some(items) = manifest.features.get(&feature) else {
            continue;
        };
        for item in items {
            if let Some(dep) = item.strip_prefix("dep:") {
                optional_dependencies.insert(dep.to_owned());
            } else if let Some((dep, dep_feature)) = item.split_once('/') {
                dependency_features
                    .entry(dep.to_owned())
                    .or_default()
                    .insert(dep_feature.to_owned());
            } else {
                stack.push(item.clone());
                optional_dependencies.insert(item.clone());
            }
        }
    }

    FeatureResolution {
        enabled_features,
        optional_dependencies,
        dependency_features,
    }
}

pub(super) fn dependency_features(
    dependency: &Dependency,
    propagated: &BTreeMap<String, BTreeSet<String>>,
) -> BTreeSet<String> {
    let mut features = dependency.features.iter().cloned().collect::<BTreeSet<_>>();
    if let Some(extra) = propagated
        .get(&dependency.alias)
        .or_else(|| propagated.get(&dependency.package))
    {
        features.extend(extra.iter().cloned());
    }
    features
}
