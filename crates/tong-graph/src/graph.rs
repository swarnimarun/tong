use crate::features::{dependency_features, resolve_features};
use crate::fetch::SourceFetcher;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use tong_core::error::{Result, TongError};
use tong_core::paths;
use tong_manifest::{Dependency, DependencySource, Manifest, SourceSpec};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PackageKey(pub PathBuf);

#[derive(Debug, Clone)]
pub struct PackageNode {
    pub key: PackageKey,
    pub manifest: Manifest,
    pub features: BTreeSet<String>,
    pub dependencies: Vec<PackageDependency>,
    pub build_dependencies: Vec<PackageDependency>,
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
        let root = graph.load_manifest_recursive(root_manifest, BTreeSet::new(), true)?;
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
        if let Some(existing) = self.packages.get_mut(&key) {
            let feature_resolution =
                resolve_features(&manifest, requested_features, default_features);
            if feature_resolution
                .enabled_features
                .is_subset(&existing.features)
            {
                return Ok(key);
            }
            let mut merged_features = existing.features.clone();
            merged_features.extend(feature_resolution.enabled_features);
            let manifest = existing.manifest.clone();
            self.packages.remove(&key);
            return self.load_manifest_recursive(manifest, merged_features, false);
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
        let mut build_dependencies = Vec::new();
        for dependency in &manifest.build_dependencies {
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
            build_dependencies.push(PackageDependency {
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
                build_dependencies,
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
        for dependency in &node.build_dependencies {
            self.visit(&dependency.key, visiting, visited)?;
        }

        visiting.remove(key);
        visited.insert(key.clone());
        Ok(())
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tong_manifest::{DependencySource, Package};

    #[test]
    fn resolves_default_features_optional_dependencies_and_dependency_features() {
        let manifest = Manifest {
            path: PathBuf::from("Cargo.toml"),
            root: PathBuf::from("."),
            kind: tong_manifest::ManifestKind::Cargo,
            package: Package {
                name: "demo".to_owned(),
                version: "0.1.0".to_owned(),
                edition: "2024".to_owned(),
            },
            features: BTreeMap::from([
                (
                    "default".to_owned(),
                    vec!["cli".to_owned(), "helper/fast".to_owned()],
                ),
                ("cli".to_owned(), vec!["dep:helper".to_owned()]),
            ]),
            sources: BTreeMap::new(),
            build_script: None,
            lib: None,
            bins: Vec::new(),
            tests: Vec::new(),
            examples: Vec::new(),
            dependencies: Vec::new(),
            build_dependencies: Vec::new(),
            workspace: None,
        };

        let resolved = resolve_features(&manifest, BTreeSet::new(), true);

        assert!(resolved.enabled_features.contains("default"));
        assert!(resolved.enabled_features.contains("cli"));
        assert!(resolved.optional_dependencies.contains("helper"));
        assert!(resolved.dependency_features["helper"].contains("fast"));
    }

    #[test]
    fn loads_path_dependency_graph() {
        let root = temp_dir("graph-path");
        write_package(&root, "app", true);
        let helper = root.join("helper");
        write_package(&helper, "helper", false);
        fs::write(
            root.join("Cargo.toml"),
            r#"
[package]
name = "app"
version = "0.1.0"
edition = "2024"

[dependencies]
helper = { path = "helper" }
"#,
        )
        .unwrap();

        let graph = ProjectGraph::load(&root.join("Cargo.toml")).unwrap();

        assert_eq!(graph.packages.len(), 2);
        let root_node = graph.package(&graph.root).unwrap();
        assert_eq!(root_node.dependencies[0].alias, "helper");
        assert_eq!(
            graph
                .package(&root_node.dependencies[0].key)
                .unwrap()
                .manifest
                .package
                .name,
            "helper"
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn merges_direct_and_propagated_dependency_features() {
        let dependency = Dependency {
            alias: "renamed".to_owned(),
            package: "actual".to_owned(),
            features: vec!["direct".to_owned()],
            default_features: true,
            optional: false,
            source: DependencySource::Path(PathBuf::from("actual")),
        };
        let propagated = BTreeMap::from([(
            "actual".to_owned(),
            BTreeSet::from(["propagated".to_owned()]),
        )]);

        let features = dependency_features(&dependency, &propagated);

        assert_eq!(
            features,
            BTreeSet::from(["direct".to_owned(), "propagated".to_owned()])
        );
    }

    #[test]
    fn reloads_package_when_later_feature_request_activates_optional_dependency() {
        let root = temp_dir("graph-feature-union");
        write_package(&root, "app", true);
        write_package(&root.join("left"), "left", false);
        write_package(&root.join("right"), "right", false);
        write_package(&root.join("shared"), "shared", false);
        write_package(&root.join("optional-helper"), "optional-helper", false);

        fs::write(
            root.join("Cargo.toml"),
            r#"
[package]
name = "app"
version = "0.1.0"
edition = "2024"

[dependencies]
left = { path = "left" }
right = { path = "right" }
"#,
        )
        .unwrap();
        fs::write(
            root.join("left/Cargo.toml"),
            r#"
[package]
name = "left"
version = "0.1.0"
edition = "2024"

[dependencies]
shared = { path = "../shared" }
"#,
        )
        .unwrap();
        fs::write(
            root.join("right/Cargo.toml"),
            r#"
[package]
name = "right"
version = "0.1.0"
edition = "2024"

[dependencies]
shared = { path = "../shared", features = ["use-helper"] }
"#,
        )
        .unwrap();
        fs::write(
            root.join("shared/Cargo.toml"),
            r#"
[package]
name = "shared"
version = "0.1.0"
edition = "2024"

[features]
use-helper = ["dep:optional-helper"]

[dependencies]
optional-helper = { path = "../optional-helper", optional = true }
"#,
        )
        .unwrap();

        let graph = ProjectGraph::load(&root.join("Cargo.toml")).unwrap();
        let shared = graph
            .packages
            .values()
            .find(|node| node.manifest.package.name == "shared")
            .unwrap();

        assert!(shared.features.contains("use-helper"));
        assert_eq!(shared.dependencies.len(), 1);
        assert_eq!(graph.packages.len(), 5);

        fs::remove_dir_all(root).unwrap();
    }

    fn write_package(root: &Path, name: &str, bin: bool) {
        fs::create_dir_all(root.join("src")).unwrap();
        if bin {
            fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
        } else {
            fs::write(root.join("src/lib.rs"), "").unwrap();
        }
        if !root.join("Cargo.toml").exists() {
            fs::write(
                root.join("Cargo.toml"),
                format!(
                    r#"
[package]
name = "{name}"
version = "0.1.0"
edition = "2024"
"#
                ),
            )
            .unwrap();
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
