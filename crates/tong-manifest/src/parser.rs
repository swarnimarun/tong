use crate::dependency::{parse_dependency_table, parse_source_spec_table};
use crate::types::*;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use tong_core::error::{Result, TongError};
use tong_core::paths;

pub fn parse_manifest(path: PathBuf, kind: ManifestKind, raw: &str) -> Result<Manifest> {
    parse_manifest_with_name_required(path, kind, raw, true)
}

pub fn parse_manifest_with_name_required(
    path: PathBuf,
    kind: ManifestKind,
    raw: &str,
    require_name: bool,
) -> Result<Manifest> {
    let root = path
        .parent()
        .ok_or_else(|| TongError::invalid_manifest(path.clone(), "manifest has no parent"))?
        .to_path_buf();

    let doc: toml::Value = raw
        .parse()
        .map_err(|e: toml::de::Error| TongError::parse(path.clone(), 0, e.message()))?;

    let package_values = doc
        .get("package")
        .and_then(|v| v.as_table())
        .cloned()
        .unwrap_or_default();

    let name = if require_name {
        required_string(&path, &package_values, "package.name")?
    } else {
        optional_string(&package_values, "name").unwrap_or_default()
    };
    let version = optional_string(&package_values, "version").unwrap_or_else(|| "0.0.0".to_owned());
    let edition = optional_string(&package_values, "edition").unwrap_or_else(|| "2021".to_owned());
    let package = Package {
        name,
        version,
        edition,
    };

    let features = parse_features(&path, doc.get("features").and_then(|v| v.as_table()))?;
    let sources = parse_sources(
        &path,
        &root,
        doc.get("tong")
            .and_then(|t| t.get("sources"))
            .and_then(|v| v.as_table()),
    )?;
    let build_script = discover_build_script(&root, &package_values);
    let lib = discover_lib(&root, &package, doc.get("lib").and_then(|v| v.as_table()));
    let bins = discover_bins(&root, &package, doc.get("bin").and_then(|v| v.as_array()));
    let tests = discover_tests(&root, doc.get("test").and_then(|v| v.as_array()));
    let examples = discover_examples(&root, doc.get("example").and_then(|v| v.as_array()));

    let dependencies = parse_dependency_section(
        &path,
        &root,
        doc.get("dependencies").and_then(|v| v.as_table()),
    )?;
    let build_dependencies = parse_dependency_section(
        &path,
        &root,
        doc.get("build-dependencies").and_then(|v| v.as_table()),
    )?;

    let workspace = parse_workspace(
        &path,
        &root,
        doc.get("workspace").and_then(|v| v.as_table()),
    )?;

    if lib.is_none() && bins.is_empty() && tests.is_empty() && examples.is_empty() {
        return Err(TongError::invalid_manifest(
            path,
            "no Rust targets found; expected src/lib.rs, src/main.rs, [lib], or [[bin]]",
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

fn parse_features(
    path: &Path,
    table: Option<&toml::value::Table>,
) -> Result<BTreeMap<String, Vec<String>>> {
    let Some(table) = table else {
        return Ok(BTreeMap::new());
    };
    let mut features = BTreeMap::new();
    for (key, value) in table {
        let arr = value.as_array().ok_or_else(|| {
            TongError::invalid_manifest(
                path.to_path_buf(),
                format!("feature `{key}` must be an array"),
            )
        })?;
        let mut strings = Vec::new();
        for item in arr {
            if let Some(s) = item.as_str() {
                strings.push(s.to_owned());
            }
        }
        features.insert(key.clone(), strings);
    }
    Ok(features)
}

fn parse_sources(
    path: &Path,
    root: &Path,
    table: Option<&toml::value::Table>,
) -> Result<BTreeMap<String, SourceSpec>> {
    let Some(table) = table else {
        return Ok(BTreeMap::new());
    };
    let mut sources = BTreeMap::new();
    for (name, value) in table {
        let values = value.as_table().ok_or_else(|| {
            TongError::invalid_manifest(
                path.to_path_buf(),
                format!("tong source `{name}` must be a table"),
            )
        })?;
        let spec = parse_source_spec_table(path, root, name.clone(), values)?;
        sources.insert(spec.0, spec.1);
    }
    Ok(sources)
}

fn parse_dependency_section(
    path: &Path,
    root: &Path,
    table: Option<&toml::value::Table>,
) -> Result<Vec<Dependency>> {
    let Some(table) = table else {
        return Ok(Vec::new());
    };
    let mut deps = Vec::new();
    for (alias, value) in table {
        let values = parse_dependency_table(value);
        let dep = crate::dependency::parse_dependency(path, root, alias.clone(), &values)?;
        deps.push(dep);
    }
    Ok(deps)
}

fn parse_workspace(
    path: &Path,
    root: &Path,
    table: Option<&toml::value::Table>,
) -> Result<Option<WorkspaceMeta>> {
    let Some(table) = table else {
        return Ok(None);
    };

    let members: Vec<String> = table
        .get("members")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_owned()))
                .collect()
        })
        .unwrap_or_default();

    let resolver = table
        .get("resolver")
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned());

    let workspace_deps = table
        .get("dependencies")
        .and_then(|v| v.as_table())
        .map(|t| {
            let mut deps = BTreeMap::new();
            for (alias, value) in t {
                let values = parse_dependency_table(value);
                if let Ok(dep) =
                    crate::dependency::parse_dependency(path, root, alias.clone(), &values)
                {
                    deps.insert(alias.clone(), dep);
                }
            }
            deps
        })
        .unwrap_or_default();

    if members.is_empty() && resolver.is_none() && workspace_deps.is_empty() {
        return Ok(None);
    }

    Ok(Some(WorkspaceMeta {
        members,
        resolver,
        dependencies: workspace_deps,
    }))
}

fn discover_build_script(root: &Path, values: &toml::value::Table) -> Option<PathBuf> {
    match values.get("build") {
        Some(toml::Value::String(path)) => Some(root.join(path)),
        Some(toml::Value::Boolean(false)) => None,
        _ => {
            let default = root.join("build.rs");
            default.exists().then_some(default)
        }
    }
}

fn discover_lib(
    root: &Path,
    package: &Package,
    values: Option<&toml::value::Table>,
) -> Option<LibTarget> {
    let default = root.join("src/lib.rs");
    if values.is_none() && !default.exists() {
        return None;
    }

    let values = values.cloned().unwrap_or_default();
    let name = optional_string(&values, "name")
        .unwrap_or_else(|| paths::normalize_crate_name(&package.name));
    let path = optional_string(&values, "path")
        .map(|path| root.join(path))
        .unwrap_or(default);
    let proc_macro = optional_bool(&values, "proc-macro").unwrap_or(false);

    Some(LibTarget {
        name,
        path,
        proc_macro,
    })
}

fn discover_bins(
    root: &Path,
    package: &Package,
    array: Option<&Vec<toml::Value>>,
) -> Vec<BinTarget> {
    if array.is_none() || array.map(|a| a.is_empty()).unwrap_or(true) {
        let default = root.join("src/main.rs");
        if default.exists() {
            return vec![BinTarget {
                name: package.name.clone(),
                path: default,
            }];
        }
        return Vec::new();
    }

    array
        .unwrap()
        .iter()
        .filter_map(|v| v.as_table())
        .map(|values| {
            let name = optional_string(values, "name").unwrap_or_else(|| package.name.clone());
            let path = optional_string(values, "path")
                .map(|path| root.join(path))
                .unwrap_or_else(|| root.join(format!("src/bin/{name}.rs")));
            BinTarget { name, path }
        })
        .collect()
}

fn discover_tests(root: &Path, array: Option<&Vec<toml::Value>>) -> Vec<TestTarget> {
    let Some(array) = array else {
        return Vec::new();
    };
    array
        .iter()
        .filter_map(|v| v.as_table())
        .map(|values| {
            let name = optional_string(values, "name").unwrap_or_else(|| "test".to_owned());
            let path = optional_string(values, "path")
                .map(|path| root.join(path))
                .unwrap_or_else(|| root.join(format!("tests/{name}.rs")));
            let required_features =
                optional_string_array(values, "required-features").unwrap_or_default();
            TestTarget {
                name,
                path,
                required_features,
            }
        })
        .collect()
}

fn discover_examples(root: &Path, array: Option<&Vec<toml::Value>>) -> Vec<ExampleTarget> {
    let Some(array) = array else {
        return Vec::new();
    };
    array
        .iter()
        .filter_map(|v| v.as_table())
        .map(|values| {
            let name = optional_string(values, "name").unwrap_or_else(|| "example".to_owned());
            let path = optional_string(values, "path")
                .map(|path| root.join(path))
                .unwrap_or_else(|| root.join(format!("examples/{name}.rs")));
            let required_features =
                optional_string_array(values, "required-features").unwrap_or_default();
            ExampleTarget {
                name,
                path,
                required_features,
            }
        })
        .collect()
}

fn required_string(path: &Path, values: &toml::value::Table, dotted_key: &str) -> Result<String> {
    let key = dotted_key.rsplit('.').next().unwrap_or(dotted_key);
    optional_string(values, key).ok_or_else(|| {
        TongError::invalid_manifest(path.to_path_buf(), format!("missing `{dotted_key}`"))
    })
}

fn optional_string(values: &toml::value::Table, key: &str) -> Option<String> {
    match values.get(key) {
        Some(toml::Value::String(value)) => Some(value.clone()),
        _ => None,
    }
}

fn optional_bool(values: &toml::value::Table, key: &str) -> Option<bool> {
    match values.get(key) {
        Some(toml::Value::Boolean(value)) => Some(*value),
        _ => None,
    }
}

fn optional_string_array(values: &toml::value::Table, key: &str) -> Option<Vec<String>> {
    match values.get(key) {
        Some(toml::Value::Array(values)) => {
            let mut result = Vec::new();
            for v in values {
                if let Some(s) = v.as_str() {
                    result.push(s.to_owned());
                }
            }
            Some(result)
        }
        _ => None,
    }
}
