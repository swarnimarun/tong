use crate::error::{Result, TongError};
use crate::fetch::SourceFetcher;
use crate::manifest::{Dependency, DependencySource, Manifest, SourceSpec};
use crate::paths;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PackageKey(pub PathBuf);

#[derive(Debug, Clone)]
pub struct PackageNode {
    pub key: PackageKey,
    pub manifest: Manifest,
    pub features: BTreeSet<String>,
    pub dependencies: Vec<PackageDependency>,
}

#[derive(Debug, Clone)]
pub struct PackageDependency {
    pub alias: String,
    pub key: PackageKey,
}

#[derive(Debug, Clone)]
pub struct ProjectGraph {
    pub root: PackageKey,
    pub packages: BTreeMap<PackageKey, PackageNode>,
    source_fetcher: SourceFetcher,
    source_overrides: BTreeMap<String, SourceSpec>,
}

impl ProjectGraph {
    pub fn load(root_manifest: &Path) -> Result<Self> {
        let root_manifest = Manifest::load(root_manifest)?;
        let store_root = root_manifest.root.join("target/tong/store/sources");
        let source_overrides = root_manifest.sources.clone();
        let mut graph = Self {
            root: PackageKey(PathBuf::new()),
            packages: BTreeMap::new(),
            source_fetcher: SourceFetcher::new(store_root),
            source_overrides,
        };
        let root = graph.load_manifest_recursive(root_manifest, BTreeSet::new(), false)?;
        graph.root = root;
        graph.validate_acyclic()?;
        Ok(graph)
    }

    pub fn package(&self, key: &PackageKey) -> Result<&PackageNode> {
        self.packages.get(key).ok_or_else(|| {
            TongError::unsupported(format!(
                "internal error: unknown package {}",
                key.0.display()
            ))
        })
    }

    fn load_manifest_recursive(
        &mut self,
        manifest: Manifest,
        requested_features: BTreeSet<String>,
        default_features: bool,
    ) -> Result<PackageKey> {
        let key = PackageKey(paths::canonicalize(&manifest.path)?);
        if self.packages.contains_key(&key) {
            return Ok(key);
        }

        for (name, spec) in &manifest.sources {
            self.source_overrides
                .entry(name.clone())
                .or_insert_with(|| spec.clone());
        }

        let feature_resolution = resolve_features(&manifest, requested_features, default_features);
        let mut dependencies = Vec::new();
        for dependency in &manifest.dependencies {
            if dependency.optional
                && !feature_resolution
                    .optional_dependencies
                    .contains(&dependency.alias)
                && !feature_resolution
                    .optional_dependencies
                    .contains(&dependency.package)
            {
                continue;
            }

            let manifest_path = self.dependency_manifest_path(&manifest, dependency)?;
            let dep_manifest = Manifest::load(&manifest_path)?;
            let dep_features =
                dependency_features(dependency, &feature_resolution.dependency_features);
            let dep_key = self.load_manifest_recursive(
                dep_manifest,
                dep_features,
                dependency.default_features,
            )?;
            dependencies.push(PackageDependency {
                alias: dependency.alias.clone(),
                key: dep_key,
            });
        }

        self.packages.insert(
            key.clone(),
            PackageNode {
                key: key.clone(),
                manifest,
                features: feature_resolution.enabled_features,
                dependencies,
            },
        );

        Ok(key)
    }

    fn dependency_manifest_path(
        &mut self,
        manifest: &Manifest,
        dependency: &Dependency,
    ) -> Result<PathBuf> {
        match &dependency.source {
            DependencySource::Path(path) => dependency_manifest_path(path),
            DependencySource::Source(spec) => {
                let source_dir = self.source_fetcher.materialize(&dependency.package, spec)?;
                dependency_manifest_path(&source_dir)
            }
            DependencySource::Registry { version } => {
                let Some(spec) = self
                    .source_overrides
                    .get(&dependency.package)
                    .or_else(|| self.source_overrides.get(&dependency.alias))
                    .cloned()
                else {
                    return Err(TongError::unsupported(format!(
                        "dependency `{}` in {} needs source override for registry version `{}`",
                        dependency.alias,
                        manifest.path.display(),
                        version
                    )));
                };
                let source_dir = self
                    .source_fetcher
                    .materialize(&dependency.package, &spec)?;
                dependency_manifest_path(&source_dir)
            }
        }
    }

    fn validate_acyclic(&self) -> Result<()> {
        let mut visiting = BTreeSet::new();
        let mut visited = BTreeSet::new();
        self.visit(&self.root, &mut visiting, &mut visited)
    }

    fn visit(
        &self,
        key: &PackageKey,
        visiting: &mut BTreeSet<PackageKey>,
        visited: &mut BTreeSet<PackageKey>,
    ) -> Result<()> {
        if visited.contains(key) {
            return Ok(());
        }
        if !visiting.insert(key.clone()) {
            let package = self
                .packages
                .get(key)
                .map(|node| node.manifest.package.name.clone())
                .unwrap_or_else(|| key.0.display().to_string());
            return Err(TongError::Cycle { package });
        }

        let node = self.package(key)?;
        for dependency in &node.dependencies {
            self.visit(&dependency.key, visiting, visited)?;
        }

        visiting.remove(key);
        visited.insert(key.clone());
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct FeatureResolution {
    enabled_features: BTreeSet<String>,
    optional_dependencies: BTreeSet<String>,
    dependency_features: BTreeMap<String, BTreeSet<String>>,
}

fn resolve_features(
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

fn dependency_features(
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

fn dependency_manifest_path(path: &Path) -> Result<PathBuf> {
    let path = paths::canonicalize(path)?;
    let tong = path.join("Tong.toml");
    if tong.exists() {
        return Ok(tong);
    }
    let cargo = path.join("Cargo.toml");
    if cargo.exists() {
        return Ok(cargo);
    }
    Err(TongError::unsupported(format!(
        "path dependency {} has no Tong.toml or Cargo.toml",
        path.display()
    )))
}
