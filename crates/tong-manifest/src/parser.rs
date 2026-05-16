use crate::dependency::{parse_dependency, parse_source_spec};
use crate::{
    BinTarget, Dependency, ExampleTarget, LibTarget, Manifest, ManifestKind, Package, TestTarget,
    Workspace,
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use toml::Value;
use tong_core::error::{Result, TongError};
use tong_core::paths;

pub(super) fn tong_extends(table: &toml::Table) -> Option<String> {
    table
        .get("tong")
        .and_then(Value::as_table)
        .and_then(|tong| tong.get("extends"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

pub(super) fn parse_manifest(
    path: PathBuf,
    kind: ManifestKind,
    table: toml::Table,
) -> Result<Manifest> {
    let root = path
        .parent()
        .ok_or_else(|| TongError::invalid_manifest(path.clone(), "manifest has no parent"))?
        .to_path_buf();

    let package_table = required_table(&path, &table, "package")?;
    let package = Package {
        name: required_string(&path, package_table, "package.name")?,
        version: string_value(package_table, "version").unwrap_or_else(|| "0.0.0".to_owned()),
        edition: string_value(package_table, "edition").unwrap_or_else(|| "2021".to_owned()),
    };

    let features = parse_features(&path, table.get("features"))?;
    let sources = parse_sources(&path, &root, table.get("tong"))?;
    let dependencies = parse_dependency_section(&path, &root, table.get("dependencies"))?;
    let build_dependencies =
        parse_dependency_section(&path, &root, table.get("build-dependencies"))?;
    let build_script = discover_build_script(&root, package_table);
    let lib = discover_lib(&root, &package, table.get("lib").and_then(Value::as_table));
    let bins = discover_bins(&root, &package, table.get("bin"));
    let tests = discover_tests(&root, table.get("test"));
    let examples = discover_examples(&root, table.get("example"));
    let workspace = parse_workspace(&path, &root, table.get("workspace"))?;

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

fn parse_features(path: &Path, value: Option<&Value>) -> Result<BTreeMap<String, Vec<String>>> {
    let Some(table) = value.and_then(Value::as_table) else {
        return Ok(BTreeMap::new());
    };
    table
        .iter()
        .map(|(key, value)| {
            string_array(path, value, &format!("[features].{key}"))
                .map(|values| (key.clone(), values))
        })
        .collect()
}

fn parse_sources(
    path: &Path,
    root: &Path,
    value: Option<&Value>,
) -> Result<BTreeMap<String, crate::SourceSpec>> {
    let Some(tong) = value.and_then(Value::as_table) else {
        return Ok(BTreeMap::new());
    };
    let Some(sources) = tong.get("sources").and_then(Value::as_table) else {
        return Ok(BTreeMap::new());
    };
    sources
        .iter()
        .map(|(name, value)| {
            let table = value.as_table().ok_or_else(|| {
                TongError::invalid_manifest(
                    path.to_path_buf(),
                    format!("[tong.sources.{name}] must be a table"),
                )
            })?;
            parse_source_spec(path, root, name.clone(), table)
        })
        .collect()
}

fn parse_dependency_section(
    path: &Path,
    root: &Path,
    value: Option<&Value>,
) -> Result<Vec<Dependency>> {
    let Some(table) = value.and_then(Value::as_table) else {
        return Ok(Vec::new());
    };
    table
        .iter()
        .map(|(alias, value)| parse_dependency(path, root, alias.clone(), value))
        .collect()
}

fn discover_build_script(root: &Path, values: &toml::Table) -> Option<PathBuf> {
    match values.get("build") {
        Some(Value::String(path)) => Some(root.join(path)),
        Some(Value::Boolean(false)) => None,
        _ => {
            let default = root.join("build.rs");
            default.exists().then_some(default)
        }
    }
}

fn discover_lib(root: &Path, package: &Package, values: Option<&toml::Table>) -> Option<LibTarget> {
    let default = root.join("src/lib.rs");
    if values.is_none() && !default.exists() {
        return None;
    }
    let values = values.cloned().unwrap_or_default();
    Some(LibTarget {
        name: string_value(&values, "name")
            .unwrap_or_else(|| paths::normalize_crate_name(&package.name)),
        path: string_value(&values, "path")
            .map(|path| root.join(path))
            .unwrap_or(default),
        proc_macro: bool_value(&values, "proc-macro").unwrap_or(false),
    })
}

fn discover_bins(root: &Path, package: &Package, value: Option<&Value>) -> Vec<BinTarget> {
    let Some(array) = value.and_then(Value::as_array) else {
        let default = root.join("src/main.rs");
        return if default.exists() {
            vec![BinTarget {
                name: package.name.clone(),
                path: default,
                required_features: Vec::new(),
            }]
        } else {
            Vec::new()
        };
    };

    array
        .iter()
        .filter_map(Value::as_table)
        .map(|values| {
            let name = string_value(values, "name").unwrap_or_else(|| package.name.clone());
            BinTarget {
                path: string_value(values, "path")
                    .map(|path| root.join(path))
                    .unwrap_or_else(|| root.join(format!("src/bin/{name}.rs"))),
                required_features: string_array_value(values, "required-features")
                    .unwrap_or_default(),
                name,
            }
        })
        .collect()
}

fn discover_tests(root: &Path, value: Option<&Value>) -> Vec<TestTarget> {
    target_array(value)
        .into_iter()
        .map(|values| {
            let name = string_value(values, "name").unwrap_or_else(|| "test".to_owned());
            TestTarget {
                path: string_value(values, "path")
                    .map(|path| root.join(path))
                    .unwrap_or_else(|| root.join(format!("tests/{name}.rs"))),
                required_features: string_array_value(values, "required-features")
                    .unwrap_or_default(),
                name,
            }
        })
        .collect()
}

fn discover_examples(root: &Path, value: Option<&Value>) -> Vec<ExampleTarget> {
    target_array(value)
        .into_iter()
        .map(|values| {
            let name = string_value(values, "name").unwrap_or_else(|| "example".to_owned());
            ExampleTarget {
                path: string_value(values, "path")
                    .map(|path| root.join(path))
                    .unwrap_or_else(|| root.join(format!("examples/{name}.rs"))),
                required_features: string_array_value(values, "required-features")
                    .unwrap_or_default(),
                name,
            }
        })
        .collect()
}

fn target_array(value: Option<&Value>) -> Vec<&toml::Table> {
    value
        .and_then(Value::as_array)
        .map(|values| values.iter().filter_map(Value::as_table).collect())
        .unwrap_or_default()
}

fn parse_workspace(path: &Path, root: &Path, value: Option<&Value>) -> Result<Option<Workspace>> {
    let Some(table) = value.and_then(Value::as_table) else {
        return Ok(None);
    };
    let members = table
        .get("members")
        .map(|value| string_array(path, value, "workspace.members"))
        .transpose()?
        .unwrap_or_default()
        .into_iter()
        .map(|member| root.join(member))
        .collect();
    let resolver = string_value(table, "resolver");
    let dependencies = parse_workspace_dependencies(path, root, table.get("dependencies"))?;
    Ok(Some(Workspace {
        members,
        resolver,
        dependencies,
    }))
}

fn parse_workspace_dependencies(
    path: &Path,
    root: &Path,
    value: Option<&Value>,
) -> Result<BTreeMap<String, Dependency>> {
    let Some(table) = value.and_then(Value::as_table) else {
        return Ok(BTreeMap::new());
    };
    table
        .iter()
        .map(|(alias, value)| {
            parse_dependency(path, root, alias.clone(), value)
                .map(|dependency| (alias.clone(), dependency))
        })
        .collect()
}

fn required_table<'a>(path: &Path, table: &'a toml::Table, key: &str) -> Result<&'a toml::Table> {
    table.get(key).and_then(Value::as_table).ok_or_else(|| {
        TongError::invalid_manifest(path.to_path_buf(), format!("missing `[{key}]`"))
    })
}

fn required_string(path: &Path, values: &toml::Table, dotted_key: &str) -> Result<String> {
    let key = dotted_key.rsplit('.').next().unwrap_or(dotted_key);
    string_value(values, key).ok_or_else(|| {
        TongError::invalid_manifest(path.to_path_buf(), format!("missing `{dotted_key}`"))
    })
}

pub(super) fn string_value(values: &toml::Table, key: &str) -> Option<String> {
    values.get(key).and_then(Value::as_str).map(str::to_owned)
}

pub(super) fn bool_value(values: &toml::Table, key: &str) -> Option<bool> {
    values.get(key).and_then(Value::as_bool)
}

pub(super) fn string_array_value(values: &toml::Table, key: &str) -> Option<Vec<String>> {
    values.get(key).and_then(string_array_lossy)
}

fn string_array(path: &Path, value: &Value, context: &str) -> Result<Vec<String>> {
    value
        .as_array()
        .ok_or_else(|| {
            TongError::invalid_manifest(
                path.to_path_buf(),
                format!("`{context}` must be an array of strings"),
            )
        })?
        .iter()
        .map(|value| {
            value.as_str().map(str::to_owned).ok_or_else(|| {
                TongError::invalid_manifest(
                    path.to_path_buf(),
                    format!("`{context}` must be an array of strings"),
                )
            })
        })
        .collect()
}

fn string_array_lossy(value: &Value) -> Option<Vec<String>> {
    value
        .as_array()?
        .iter()
        .map(|value| value.as_str().map(str::to_owned))
        .collect()
}
