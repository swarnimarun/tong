use crate::{Dependency, DependencySource, SourceSpec};
use std::collections::BTreeMap;
use std::path::Path;
use tong_core::error::{Result, TongError};
use tong_core::paths;

pub(super) fn parse_dependency(
    manifest: &Path,
    root: &Path,
    alias: String,
    values: &BTreeMap<String, toml::Value>,
) -> Result<Dependency> {
    let package = optional_string(values, "package").unwrap_or_else(|| alias.clone());
    let features = optional_string_array(values, "features").unwrap_or_default();
    let default_features = optional_bool(values, "default-features").unwrap_or(true);
    let optional = optional_bool(values, "optional").unwrap_or(false);

    let source = if let Some(path) = optional_string(values, "path") {
        DependencySource::Path(root.join(path))
    } else if values.contains_key("git") {
        DependencySource::Source(SourceSpec::Git {
            url: required_source_url(manifest, root, values, "git")?,
            rev: optional_string(values, "rev")
                .or_else(|| optional_string(values, "tag"))
                .or_else(|| optional_string(values, "branch")),
            subdir: optional_string(values, "subdir"),
        })
    } else if values.contains_key("tar") {
        DependencySource::Source(SourceSpec::Tar {
            url: required_source_url(manifest, root, values, "tar")?,
            sha256: optional_string(values, "sha256"),
            strip_prefix: optional_string(values, "strip-prefix"),
            subdir: optional_string(values, "subdir"),
        })
    } else if values.contains_key("zip") {
        DependencySource::Source(SourceSpec::Zip {
            url: required_source_url(manifest, root, values, "zip")?,
            sha256: optional_string(values, "sha256"),
            strip_prefix: optional_string(values, "strip-prefix"),
            subdir: optional_string(values, "subdir"),
        })
    } else if let Some(version) = optional_string(values, "version") {
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

pub(super) fn parse_dependency_table(value: &toml::Value) -> BTreeMap<String, toml::Value> {
    match value {
        toml::Value::Table(table) => table.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        toml::Value::String(version) => {
            let mut table = BTreeMap::new();
            table.insert("version".to_owned(), toml::Value::String(version.clone()));
            table
        }
        other => {
            let mut table = BTreeMap::new();
            table.insert("version".to_owned(), other.clone());
            table
        }
    }
}

pub(super) fn parse_source_spec_table(
    manifest: &Path,
    root: &Path,
    name: String,
    values: &toml::value::Table,
) -> Result<(String, SourceSpec)> {
    let spec = if values.contains_key("git") {
        SourceSpec::Git {
            url: required_source_url_table(manifest, root, values, "git")?,
            rev: optional_string_table(values, "rev")
                .or_else(|| optional_string_table(values, "tag"))
                .or_else(|| optional_string_table(values, "branch")),
            subdir: optional_string_table(values, "subdir"),
        }
    } else if values.contains_key("tar") {
        SourceSpec::Tar {
            url: required_source_url_table(manifest, root, values, "tar")?,
            sha256: optional_string_table(values, "sha256"),
            strip_prefix: optional_string_table(values, "strip-prefix"),
            subdir: optional_string_table(values, "subdir"),
        }
    } else if values.contains_key("zip") {
        SourceSpec::Zip {
            url: required_source_url_table(manifest, root, values, "zip")?,
            sha256: optional_string_table(values, "sha256"),
            strip_prefix: optional_string_table(values, "strip-prefix"),
            subdir: optional_string_table(values, "subdir"),
        }
    } else {
        return Err(TongError::invalid_manifest(
            manifest.to_path_buf(),
            format!("tong source `{name}` needs git, tar, or zip"),
        ));
    };

    Ok((name, spec))
}

fn required_source_url(
    path: &Path,
    root: &Path,
    values: &BTreeMap<String, toml::Value>,
    key: &str,
) -> Result<String> {
    let value = optional_string(values, key).ok_or_else(|| {
        TongError::invalid_manifest(path.to_path_buf(), format!("missing `{key}`"))
    })?;
    Ok(normalize_source_url(root, &value))
}

fn required_source_url_table(
    path: &Path,
    root: &Path,
    values: &toml::value::Table,
    key: &str,
) -> Result<String> {
    let value = optional_string_table(values, key).ok_or_else(|| {
        TongError::invalid_manifest(path.to_path_buf(), format!("missing `{key}`"))
    })?;
    Ok(normalize_source_url(root, &value))
}

fn normalize_source_url(root: &Path, value: &str) -> String {
    if value.contains("://") || Path::new(value).is_absolute() {
        value.to_owned()
    } else {
        paths::display_path(&root.join(value))
    }
}

fn optional_string(values: &BTreeMap<String, toml::Value>, key: &str) -> Option<String> {
    match values.get(key) {
        Some(toml::Value::String(value)) => Some(value.clone()),
        _ => None,
    }
}

fn optional_string_table(values: &toml::value::Table, key: &str) -> Option<String> {
    match values.get(key) {
        Some(toml::Value::String(value)) => Some(value.clone()),
        _ => None,
    }
}

fn optional_bool(values: &BTreeMap<String, toml::Value>, key: &str) -> Option<bool> {
    match values.get(key) {
        Some(toml::Value::Boolean(value)) => Some(*value),
        _ => None,
    }
}

fn optional_string_array(values: &BTreeMap<String, toml::Value>, key: &str) -> Option<Vec<String>> {
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
