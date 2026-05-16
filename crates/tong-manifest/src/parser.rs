use crate::dependency::parse_dependency;
use crate::types::{
    BinTarget, Dependency, ExampleTarget, LibTarget, Manifest, ManifestKind, Package, SourceSpec,
    TestTarget, TongConfig, Workspace,
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use tong_core::error::{Result, TongError};
use tong_core::paths;

pub(crate) fn parse_manifest_toml(
    path: PathBuf,
    kind: ManifestKind,
    raw: &str,
) -> Result<Manifest> {
    let root = path
        .parent()
        .ok_or_else(|| {
            TongError::invalid_manifest(path.clone(), "manifest has no parent directory")
        })?
        .to_path_buf();

    let table: toml::Table = toml::from_str(raw)
        .map_err(|e| TongError::invalid_manifest(path.clone(), format!("toml parse error: {e}")))?;

    let package = extract_package(&path, &table)?;
    let dependencies = extract_dependencies(&path, &root, &table, "dependencies")?;
    let build_dependencies = extract_dependencies(&path, &root, &table, "build-dependencies")?;
    let features = extract_features(&table);
    let lib = extract_lib(&root, &package, &table);
    let bins = extract_bins(&root, &package, &table);
    let tests = extract_test_targets(&root, &package, &table);
    let examples = extract_example_targets(&root, &package, &table);
    let build_script = extract_build_script(&root, &table);
    let sources = extract_sources(&path, &root, &table)?;
    let workspace = extract_workspace(&table);
    let tong = extract_tong_config(&table);

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
        tong,
    })
}

pub(crate) fn parse_tong_extends(raw: &str, path: &Path) -> Result<Option<String>> {
    let table: toml::Table = toml::from_str(raw).map_err(|e| {
        TongError::invalid_manifest(path.to_path_buf(), format!("toml parse error: {e}"))
    })?;
    Ok(table
        .get("tong")
        .and_then(|v| v.get("extends"))
        .and_then(|v| v.as_str())
        .map(str::to_owned))
}

fn extract_package(path: &Path, table: &toml::Table) -> Result<Package> {
    let package = table
        .get("package")
        .and_then(|v| v.as_table())
        .ok_or_else(|| TongError::invalid_manifest(path.to_path_buf(), "missing [package]"))?;

    let name = package
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| TongError::invalid_manifest(path.to_path_buf(), "missing package.name"))?
        .to_owned();
    let version = package
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("0.0.0")
        .to_owned();
    let edition = package
        .get("edition")
        .and_then(|v| v.as_str())
        .unwrap_or("2021")
        .to_owned();

    Ok(Package {
        name,
        version,
        edition,
    })
}

fn extract_dependencies(
    path: &Path,
    root: &Path,
    table: &toml::Table,
    section: &str,
) -> Result<Vec<Dependency>> {
    let Some(section_table) = table.get(section).and_then(|v| v.as_table()) else {
        return Ok(Vec::new());
    };

    let mut dependencies = Vec::new();
    for (alias, value) in section_table {
        let values = dependency_value_to_table(value);
        dependencies.push(parse_dependency(path, root, alias.clone(), values)?);
    }
    Ok(dependencies)
}

fn dependency_value_to_table(value: &toml::Value) -> BTreeMap<String, toml::Value> {
    match value {
        toml::Value::Table(table) => table.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        toml::Value::String(s) => {
            let mut map = BTreeMap::new();
            map.insert("version".to_owned(), toml::Value::String(s.clone()));
            map
        }
        _ => {
            let mut map = BTreeMap::new();
            map.insert("version".to_owned(), value.clone());
            map
        }
    }
}

fn extract_features(table: &toml::Table) -> BTreeMap<String, Vec<String>> {
    let Some(features) = table.get("features").and_then(|v| v.as_table()) else {
        return BTreeMap::new();
    };

    let mut map = BTreeMap::new();
    for (key, value) in features {
        if let Some(items) = value.as_array() {
            let strings: Vec<String> = items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect();
            map.insert(key.clone(), strings);
        }
    }
    map
}

fn extract_lib(root: &Path, package: &Package, table: &toml::Table) -> Option<LibTarget> {
    let default = root.join("src/lib.rs");
    let lib_table = table.get("lib").and_then(|v| v.as_table());

    if lib_table.is_none() && !default.exists() {
        return None;
    }

    let name = lib_table
        .and_then(|t| t.get("name"))
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .unwrap_or_else(|| paths::normalize_crate_name(&package.name));
    let path = lib_table
        .and_then(|t| t.get("path"))
        .and_then(|v| v.as_str())
        .map(|p| root.join(p))
        .unwrap_or(default);
    let proc_macro = lib_table
        .and_then(|t| t.get("proc-macro"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let required_features = lib_table
        .and_then(|t| t.get("required-features"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();

    Some(LibTarget {
        name,
        path,
        proc_macro,
        required_features,
    })
}

fn extract_bins(root: &Path, package: &Package, table: &toml::Table) -> Vec<BinTarget> {
    let Some(bins) = table.get("bin").and_then(|v| v.as_array()) else {
        let default = root.join("src/main.rs");
        if default.exists() {
            return vec![BinTarget {
                name: package.name.clone(),
                path: default,
                required_features: Vec::new(),
            }];
        }
        return Vec::new();
    };

    bins.iter()
        .filter_map(|entry| entry.as_table())
        .map(|values| {
            let name = values
                .get("name")
                .and_then(|v| v.as_str())
                .map(str::to_owned)
                .unwrap_or_else(|| package.name.clone());
            let path = values
                .get("path")
                .and_then(|v| v.as_str())
                .map(|p| root.join(p))
                .unwrap_or_else(|| root.join(format!("src/bin/{name}.rs")));
            let required_features = values
                .get("required-features")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            BinTarget {
                name,
                path,
                required_features,
            }
        })
        .collect()
}

fn extract_test_targets(root: &Path, _package: &Package, table: &toml::Table) -> Vec<TestTarget> {
    let Some(tests) = table.get("test").and_then(|v| v.as_array()) else {
        return Vec::new();
    };

    tests
        .iter()
        .filter_map(|entry| entry.as_table())
        .enumerate()
        .map(|(i, values)| {
            let name = values
                .get("name")
                .and_then(|v| v.as_str())
                .map(str::to_owned)
                .unwrap_or_else(|| format!("test_{i}"));
            let path = values
                .get("path")
                .and_then(|v| v.as_str())
                .map(|p| root.join(p))
                .unwrap_or_else(|| root.join(format!("tests/{name}.rs")));
            let required_features = values
                .get("required-features")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            TestTarget {
                name,
                path,
                required_features,
            }
        })
        .collect()
}

fn extract_example_targets(
    root: &Path,
    package: &Package,
    table: &toml::Table,
) -> Vec<ExampleTarget> {
    let Some(examples) = table.get("example").and_then(|v| v.as_array()) else {
        return Vec::new();
    };

    examples
        .iter()
        .filter_map(|entry| entry.as_table())
        .map(|values| {
            let name = values
                .get("name")
                .and_then(|v| v.as_str())
                .map(str::to_owned)
                .unwrap_or_else(|| package.name.clone());
            let path = values
                .get("path")
                .and_then(|v| v.as_str())
                .map(|p| root.join(p))
                .unwrap_or_else(|| root.join(format!("examples/{name}.rs")));
            let required_features = values
                .get("required-features")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            let crate_type = values
                .get("crate-type")
                .and_then(|v| v.as_str())
                .map(str::to_owned);
            ExampleTarget {
                name,
                path,
                required_features,
                crate_type,
            }
        })
        .collect()
}

fn extract_build_script(root: &Path, table: &toml::Table) -> Option<PathBuf> {
    let custom = table
        .get("package")
        .and_then(|v| v.get("build"))
        .and_then(|v| v.as_str())
        .map(|p| root.join(p));

    match custom {
        Some(path) => Some(path),
        None => {
            let default = root.join("build.rs");
            default.exists().then_some(default)
        }
    }
}

fn extract_sources(
    path: &Path,
    root: &Path,
    table: &toml::Table,
) -> Result<BTreeMap<String, SourceSpec>> {
    let Some(tong) = table.get("tong").and_then(|v| v.as_table()) else {
        return Ok(BTreeMap::new());
    };
    let sources = match tong.get("sources").and_then(|v| v.as_table()) {
        Some(s) => s,
        None => return Ok(BTreeMap::new()),
    };

    let mut map = BTreeMap::new();
    for (name, value) in sources {
        let spec = parse_source_entry(path, root, name, value)?;
        map.insert(name.clone(), spec);
    }
    Ok(map)
}

fn parse_source_entry(
    path: &Path,
    root: &Path,
    name: &str,
    value: &toml::Value,
) -> Result<SourceSpec> {
    let table = value.as_table().ok_or_else(|| {
        TongError::invalid_manifest(
            path.to_path_buf(),
            format!("tong.sources.{name} must be a table"),
        )
    })?;

    if table.contains_key("git") {
        Ok(SourceSpec::Git {
            url: required_source_url(path, root, table, "git")?,
            rev: table
                .get("rev")
                .or_else(|| table.get("tag"))
                .or_else(|| table.get("branch"))
                .and_then(|v| v.as_str())
                .map(str::to_owned),
            subdir: table
                .get("subdir")
                .and_then(|v| v.as_str())
                .map(str::to_owned),
        })
    } else if table.contains_key("tar") {
        Ok(SourceSpec::Tar {
            url: required_source_url(path, root, table, "tar")?,
            sha256: table
                .get("sha256")
                .and_then(|v| v.as_str())
                .map(str::to_owned),
            strip_prefix: table
                .get("strip-prefix")
                .and_then(|v| v.as_str())
                .map(str::to_owned),
            subdir: table
                .get("subdir")
                .and_then(|v| v.as_str())
                .map(str::to_owned),
        })
    } else if table.contains_key("zip") {
        Ok(SourceSpec::Zip {
            url: required_source_url(path, root, table, "zip")?,
            sha256: table
                .get("sha256")
                .and_then(|v| v.as_str())
                .map(str::to_owned),
            strip_prefix: table
                .get("strip-prefix")
                .and_then(|v| v.as_str())
                .map(str::to_owned),
            subdir: table
                .get("subdir")
                .and_then(|v| v.as_str())
                .map(str::to_owned),
        })
    } else {
        Err(TongError::invalid_manifest(
            path.to_path_buf(),
            format!("tong source `{name}` needs git, tar, or zip"),
        ))
    }
}

fn extract_workspace(table: &toml::Table) -> Option<Workspace> {
    let ws = table.get("workspace")?.as_table()?;
    let members = ws
        .get("members")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let resolver = ws
        .get("resolver")
        .and_then(|v| v.as_str())
        .map(str::to_owned);
    let dependencies = BTreeMap::new();

    Some(Workspace {
        members,
        resolver,
        dependencies,
    })
}

fn extract_tong_config(table: &toml::Table) -> Option<TongConfig> {
    let tong = table.get("tong")?.as_table()?;
    Some(TongConfig {
        sandbox: tong
            .get("sandbox")
            .and_then(|v| v.as_str())
            .map(str::to_owned),
        extends: tong
            .get("extends")
            .and_then(|v| v.as_str())
            .map(str::to_owned),
    })
}

fn required_source_url(
    path: &Path,
    root: &Path,
    values: &toml::Table,
    key: &str,
) -> Result<String> {
    let value = values.get(key).and_then(|v| v.as_str()).ok_or_else(|| {
        TongError::invalid_manifest(path.to_path_buf(), format!("missing `{key}`"))
    })?;
    Ok(normalize_source_url(root, value))
}

fn normalize_source_url(root: &Path, value: &str) -> String {
    if value.contains("://") || Path::new(value).is_absolute() {
        value.to_owned()
    } else {
        paths::display_path(&root.join(value))
    }
}
