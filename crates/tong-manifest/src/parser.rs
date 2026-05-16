use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use tong_core::error::{Result, TongError};
use tong_core::paths;

use crate::types::*;

pub fn parse_manifest(path: PathBuf, kind: ManifestKind, raw: &str) -> Result<Manifest> {
    let root = path
        .parent()
        .ok_or_else(|| TongError::invalid_manifest(path.clone(), "manifest has no parent"))?
        .to_path_buf();

    let table: toml::Table = raw.parse().map_err(|err| {
        let message = format!("TOML parse error: {err}");
        TongError::invalid_manifest(path.clone(), message)
    })?;

    let package = parse_package(&path, &table)?;
    let features = parse_features(&table);
    let sources = parse_sources(&path, &root, &table)?;
    let dependencies = parse_dependencies(&path, &root, &table, "dependencies")?;
    let build_dependencies = parse_dependencies(&path, &root, &table, "build-dependencies")?;
    let workspace = parse_workspace(&path, &root, &table)?;

    let lib = parse_lib(&path, &root, &package, &table)?;
    let bins = parse_bins(&path, &root, &package, &table)?;
    let tests = parse_tests(&path, &root, &package, &table)?;
    let examples = parse_examples(&path, &root, &package, &table)?;
    let build_script = discover_build_script(&root, &package, &table);

    if lib.is_none() && bins.is_empty() && tests.is_empty() && examples.is_empty() {
        return Err(TongError::invalid_manifest(
            path,
            "no Rust targets found; expected src/lib.rs, src/main.rs, [lib], [[bin]], [[test]], or [[example]]",
        ));
    }

    Ok(Manifest {
        path,
        root,
        kind,
        package,
        features,
        sources,
        build_script,
        lib,
        bins,
        tests,
        examples,
        dependencies,
        build_dependencies,
        workspace,
    })
}

fn parse_package(path: &Path, table: &toml::Table) -> Result<Package> {
    let package = table
        .get("package")
        .and_then(|v| v.as_table())
        .ok_or_else(|| {
            TongError::invalid_manifest(path.to_path_buf(), "missing [package] section")
        })?;

    let name = package
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| TongError::invalid_manifest(path.to_path_buf(), "missing package.name"))?;

    let version = package
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("0.0.0");

    let edition = package
        .get("edition")
        .and_then(|v| v.as_str())
        .unwrap_or("2021");

    Ok(Package {
        name: name.to_owned(),
        version: version.to_owned(),
        edition: edition.to_owned(),
    })
}

fn parse_features(table: &toml::Table) -> BTreeMap<String, Vec<String>> {
    let mut features = BTreeMap::new();
    if let Some(section) = table.get("features").and_then(|v| v.as_table()) {
        for (key, value) in section {
            if let Some(arr) = value.as_array() {
                let strings: Vec<String> = arr
                    .iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect();
                features.insert(key.clone(), strings);
            }
        }
    }
    features
}

fn parse_dependencies(
    path: &Path,
    root: &Path,
    table: &toml::Table,
    section_name: &str,
) -> Result<Vec<Dependency>> {
    let mut dependencies = Vec::new();

    if let Some(section) = table.get(section_name).and_then(|v| v.as_table()) {
        for (alias, value) in section {
            let dep = parse_dependency_value(path, root, alias.clone(), value)?;
            dependencies.push(dep);
        }
    }

    // Also handle [section.alias] tables
    for (key, value) in table {
        if let Some(suffix) = key.strip_prefix(&format!("{section_name}."))
            && let Some(sub_table) = value.as_table()
        {
            let dep = parse_dependency_table(path, root, suffix.to_owned(), sub_table)?;
            dependencies.push(dep);
        }
    }

    Ok(dependencies)
}

fn parse_dependency_value(
    path: &Path,
    root: &Path,
    alias: String,
    value: &toml::Value,
) -> Result<Dependency> {
    match value {
        toml::Value::String(version) => Ok(Dependency {
            alias: alias.clone(),
            package: alias,
            features: Vec::new(),
            default_features: true,
            optional: false,
            source: DependencySource::Registry {
                version: version.clone(),
            },
        }),
        toml::Value::Table(table) => parse_dependency_table(path, root, alias, table),
        _ => Err(TongError::invalid_manifest(
            path.to_path_buf(),
            format!("invalid dependency value for `{alias}`"),
        )),
    }
}

fn parse_dependency_table(
    path: &Path,
    root: &Path,
    alias: String,
    table: &toml::Table,
) -> Result<Dependency> {
    let package = table
        .get("package")
        .and_then(|v| v.as_str())
        .unwrap_or(&alias)
        .to_owned();

    let features = table
        .get("features")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let default_features = table
        .get("default-features")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    let optional = table
        .get("optional")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let source = if let Some(path_str) = table.get("path").and_then(|v| v.as_str()) {
        DependencySource::Path(root.join(path_str))
    } else if table.contains_key("git") {
        DependencySource::Source(SourceSpec::Git {
            url: required_source_url(path, table, "git")?,
            rev: optional_string(table, "rev")
                .or_else(|| optional_string(table, "tag"))
                .or_else(|| optional_string(table, "branch")),
            subdir: optional_string(table, "subdir"),
        })
    } else if table.contains_key("tar") {
        DependencySource::Source(SourceSpec::Tar {
            url: required_source_url(path, table, "tar")?,
            sha256: optional_string(table, "sha256"),
            strip_prefix: optional_string(table, "strip-prefix"),
            subdir: optional_string(table, "subdir"),
        })
    } else if table.contains_key("zip") {
        DependencySource::Source(SourceSpec::Zip {
            url: required_source_url(path, table, "zip")?,
            sha256: optional_string(table, "sha256"),
            strip_prefix: optional_string(table, "strip-prefix"),
            subdir: optional_string(table, "subdir"),
        })
    } else if let Some(version) = optional_string(table, "version") {
        DependencySource::Registry { version }
    } else {
        DependencySource::Registry {
            version: "*".to_owned(),
        }
    };

    Ok(Dependency {
        alias,
        package,
        features,
        default_features,
        optional,
        source,
    })
}

fn parse_sources(
    path: &Path,
    root: &Path,
    table: &toml::Table,
) -> Result<BTreeMap<String, SourceSpec>> {
    let mut sources = BTreeMap::new();

    let tong = match table.get("tong").and_then(|v| v.as_table()) {
        Some(t) => t,
        None => return Ok(sources),
    };

    let sources_table = match tong.get("sources").and_then(|v| v.as_table()) {
        Some(s) => s,
        None => return Ok(sources),
    };

    for (name, value) in sources_table {
        if let Some(sub_table) = value.as_table() {
            let spec = parse_source_spec_table(path, root, name, sub_table)?;
            sources.insert(name.clone(), spec);
        }
    }

    Ok(sources)
}

fn parse_source_spec_table(
    path: &Path,
    _root: &Path,
    name: &str,
    table: &toml::Table,
) -> Result<SourceSpec> {
    let spec = if table.contains_key("git") {
        SourceSpec::Git {
            url: required_source_url(path, table, "git")?,
            rev: optional_string(table, "rev")
                .or_else(|| optional_string(table, "tag"))
                .or_else(|| optional_string(table, "branch")),
            subdir: optional_string(table, "subdir"),
        }
    } else if table.contains_key("tar") {
        SourceSpec::Tar {
            url: required_source_url(path, table, "tar")?,
            sha256: optional_string(table, "sha256"),
            strip_prefix: optional_string(table, "strip-prefix"),
            subdir: optional_string(table, "subdir"),
        }
    } else if table.contains_key("zip") {
        SourceSpec::Zip {
            url: required_source_url(path, table, "zip")?,
            sha256: optional_string(table, "sha256"),
            strip_prefix: optional_string(table, "strip-prefix"),
            subdir: optional_string(table, "subdir"),
        }
    } else {
        return Err(TongError::invalid_manifest(
            path.to_path_buf(),
            format!("tong source `{name}` needs git, tar, or zip"),
        ));
    };

    Ok(spec)
}

fn parse_lib(
    _path: &Path,
    root: &Path,
    package: &Package,
    table: &toml::Table,
) -> Result<Option<LibTarget>> {
    let default = root.join("src/lib.rs");

    if let Some(lib_table) = table.get("lib").and_then(|v| v.as_table()) {
        let name = optional_string(lib_table, "name")
            .unwrap_or_else(|| paths::normalize_crate_name(&package.name));
        let path = optional_string(lib_table, "path")
            .map(|p| root.join(p))
            .unwrap_or(default);
        let proc_macro = lib_table
            .get("proc-macro")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        return Ok(Some(LibTarget {
            name,
            path,
            proc_macro,
        }));
    }

    if default.exists() {
        Ok(Some(LibTarget {
            name: paths::normalize_crate_name(&package.name),
            path: default,
            proc_macro: false,
        }))
    } else {
        Ok(None)
    }
}

fn parse_bins(
    _path: &Path,
    root: &Path,
    package: &Package,
    table: &toml::Table,
) -> Result<Vec<BinTarget>> {
    let mut bins = Vec::new();

    if let Some(bin_array) = table.get("bin").and_then(|v| v.as_array()) {
        for item in bin_array {
            if let Some(bin_table) = item.as_table() {
                let name =
                    optional_string(bin_table, "name").unwrap_or_else(|| package.name.clone());
                let path = optional_string(bin_table, "path")
                    .map(|p| root.join(p))
                    .unwrap_or_else(|| root.join(format!("src/bin/{name}.rs")));
                bins.push(BinTarget { name, path });
            }
        }
    }

    if bins.is_empty() {
        let default = root.join("src/main.rs");
        if default.exists() {
            bins.push(BinTarget {
                name: package.name.clone(),
                path: default,
            });
        }
    }

    Ok(bins)
}

fn parse_tests(
    _path: &Path,
    root: &Path,
    package: &Package,
    table: &toml::Table,
) -> Result<Vec<TestTarget>> {
    let mut tests = Vec::new();

    if let Some(test_array) = table.get("test").and_then(|v| v.as_array()) {
        for item in test_array {
            if let Some(test_table) = item.as_table() {
                let name =
                    optional_string(test_table, "name").unwrap_or_else(|| package.name.clone());
                let path = optional_string(test_table, "path")
                    .map(|p| root.join(p))
                    .unwrap_or_else(|| root.join(format!("tests/{name}.rs")));
                let required_features = test_table
                    .get("required-features")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                tests.push(TestTarget {
                    name,
                    path,
                    required_features,
                });
            }
        }
    }

    Ok(tests)
}

fn parse_examples(
    _path: &Path,
    root: &Path,
    package: &Package,
    table: &toml::Table,
) -> Result<Vec<ExampleTarget>> {
    let mut examples = Vec::new();

    if let Some(example_array) = table.get("example").and_then(|v| v.as_array()) {
        for item in example_array {
            if let Some(example_table) = item.as_table() {
                let name =
                    optional_string(example_table, "name").unwrap_or_else(|| package.name.clone());
                let path = optional_string(example_table, "path")
                    .map(|p| root.join(p))
                    .unwrap_or_else(|| root.join(format!("examples/{name}.rs")));
                let required_features = example_table
                    .get("required-features")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                examples.push(ExampleTarget {
                    name,
                    path,
                    required_features,
                });
            }
        }
    }

    Ok(examples)
}

fn parse_workspace(
    path: &Path,
    root: &Path,
    table: &toml::Table,
) -> Result<Option<WorkspaceMetadata>> {
    let workspace_table = match table.get("workspace").and_then(|v| v.as_table()) {
        Some(t) => t,
        None => return Ok(None),
    };

    let members = workspace_table
        .get("members")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let resolver = optional_string(workspace_table, "resolver");

    let deps_vec = parse_dependencies(path, root, workspace_table, "dependencies")?;
    let dependencies = deps_vec.into_iter().map(|d| (d.alias.clone(), d)).collect();

    Ok(Some(WorkspaceMetadata {
        members,
        resolver,
        dependencies,
    }))
}

fn discover_build_script(root: &Path, _package: &Package, table: &toml::Table) -> Option<PathBuf> {
    let build_value = table
        .get("package")
        .and_then(|v| v.as_table())?
        .get("build");

    match build_value {
        Some(toml::Value::String(path)) => Some(root.join(path)),
        Some(toml::Value::Boolean(false)) => None,
        _ => {
            let default = root.join("build.rs");
            default.exists().then_some(default)
        }
    }
}

fn required_source_url(path: &Path, table: &toml::Table, key: &str) -> Result<String> {
    let value = optional_string(table, key).ok_or_else(|| {
        TongError::invalid_manifest(path.to_path_buf(), format!("missing `{key}`"))
    })?;
    Ok(value)
}

fn optional_string(table: &toml::Table, key: &str) -> Option<String> {
    table.get(key).and_then(|v| v.as_str()).map(String::from)
}

pub fn parse_tong_overlay(path: &Path, raw: &str) -> Result<Manifest> {
    let root = path
        .parent()
        .ok_or_else(|| TongError::invalid_manifest(path.to_path_buf(), "manifest has no parent"))?
        .to_path_buf();

    let table: toml::Table = raw.parse().map_err(|err| {
        let message = format!("TOML parse error: {err}");
        TongError::invalid_manifest(path.to_path_buf(), message)
    })?;

    let package = parse_package(path, &table).unwrap_or(Package {
        name: String::new(),
        version: String::new(),
        edition: String::new(),
    });
    let features = parse_features(&table);
    let sources = parse_sources(path, &root, &table)?;
    let dependencies = parse_dependencies(path, &root, &table, "dependencies")?;
    let build_dependencies = parse_dependencies(path, &root, &table, "build-dependencies")?;
    let workspace = parse_workspace(path, &root, &table)?;

    let lib = parse_lib(path, &root, &package, &table).unwrap_or(None);
    let bins = parse_bins(path, &root, &package, &table).unwrap_or_default();
    let tests = parse_tests(path, &root, &package, &table).unwrap_or_default();
    let examples = parse_examples(path, &root, &package, &table).unwrap_or_default();
    let build_script = discover_build_script(&root, &package, &table);

    Ok(Manifest {
        path: path.to_path_buf(),
        root,
        kind: ManifestKind::Tong,
        package,
        features,
        sources,
        build_script,
        lib,
        bins,
        tests,
        examples,
        dependencies,
        build_dependencies,
        workspace,
    })
}

pub fn parse_tong_extends(path: &Path, raw: &str) -> Result<Option<String>> {
    let table: toml::Table = raw.parse().map_err(|err| {
        TongError::invalid_manifest(path.to_path_buf(), format!("TOML parse error: {err}"))
    })?;

    // Check root-level `extends` first (common for Tong.toml)
    if let Some(extends) = table.get("extends").and_then(|v| v.as_str()) {
        return Ok(Some(extends.to_owned()));
    }

    // Also check inside [tong] section
    if let Some(tong_table) = table.get("tong").and_then(|v| v.as_table())
        && let Some(extends) = tong_table.get("extends").and_then(|v| v.as_str())
    {
        return Ok(Some(extends.to_owned()));
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn parses_simple_cargo_toml() {
        let root = temp_dir("parser-simple");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
        fs::write(
            root.join("Cargo.toml"),
            r#"
[package]
name = "demo"
version = "0.1.0"
edition = "2024"

[dependencies]
serde = "1.0"
helper = { path = "helper" }
"#,
        )
        .unwrap();

        let manifest = Manifest::load(&root.join("Cargo.toml")).unwrap();
        assert_eq!(manifest.package.name, "demo");
        assert_eq!(manifest.dependencies.len(), 2);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn parses_toml_with_features_and_sources() {
        let root = temp_dir("parser-features");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "").unwrap();
        fs::write(
            root.join("Tong.toml"),
            r#"
[package]
name = "demo"
version = "0.1.0"

[features]
default = ["cli"]
cli = []

[tong.sources.serde]
git = "https://github.com/serde-rs/serde"
rev = "v1.0"
"#,
        )
        .unwrap();

        let manifest = Manifest::load(&root.join("Tong.toml")).unwrap();
        assert!(manifest.features.contains_key("default"));
        assert!(manifest.sources.contains_key("serde"));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn parses_build_dependencies() {
        let root = temp_dir("parser-build-deps");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
        fs::write(
            root.join("Cargo.toml"),
            r#"
[package]
name = "demo"
version = "0.1.0"

[build-dependencies]
cc = "1.0"
"#,
        )
        .unwrap();

        let manifest = Manifest::load(&root.join("Cargo.toml")).unwrap();
        assert_eq!(manifest.build_dependencies.len(), 1);
        assert_eq!(manifest.build_dependencies[0].alias, "cc");

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn parses_workspace_metadata() {
        let root = temp_dir("parser-workspace");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "").unwrap();
        fs::write(
            root.join("Cargo.toml"),
            r#"
[package]
name = "demo"
version = "0.1.0"

[workspace]
members = ["crates/*"]
resolver = "2"

[workspace.dependencies]
serde = "1.0"
"#,
        )
        .unwrap();

        let manifest = Manifest::load(&root.join("Cargo.toml")).unwrap();
        let workspace = manifest.workspace.as_ref().unwrap();
        assert_eq!(workspace.members, vec!["crates/*"]);
        assert_eq!(workspace.resolver, Some("2".to_owned()));
        assert_eq!(workspace.dependencies.len(), 1);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn parses_tests_and_examples() {
        let root = temp_dir("parser-tests-examples");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
        fs::write(
            root.join("Cargo.toml"),
            r#"
[package]
name = "demo"
version = "0.1.0"

[[test]]
name = "integration"
path = "tests/integration.rs"
required-features = ["test-feature"]

[[example]]
name = "demo-example"
path = "examples/demo.rs"
"#,
        )
        .unwrap();

        let manifest = Manifest::load(&root.join("Cargo.toml")).unwrap();
        assert_eq!(manifest.tests.len(), 1);
        assert_eq!(manifest.tests[0].name, "integration");
        assert_eq!(manifest.tests[0].required_features, vec!["test-feature"]);
        assert_eq!(manifest.examples.len(), 1);
        assert_eq!(manifest.examples[0].name, "demo-example");

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn tong_extends_cargo_toml() {
        let root = temp_dir("parser-extends");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
        fs::write(
            root.join("Cargo.toml"),
            r#"
[package]
name = "demo"
version = "0.1.0"
edition = "2024"
"#,
        )
        .unwrap();
        fs::write(
            root.join("Tong.toml"),
            r#"
extends = "Cargo.toml"

[tong.sources.serde]
git = "https://github.com/serde-rs/serde"
"#,
        )
        .unwrap();

        let manifest = Manifest::load(&root.join("Tong.toml")).unwrap();
        assert_eq!(manifest.package.name, "demo");
        assert!(manifest.sources.contains_key("serde"));

        fs::remove_dir_all(root).unwrap();
    }

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("tong-{name}-{}-{nanos}", std::process::id()))
    }
}
