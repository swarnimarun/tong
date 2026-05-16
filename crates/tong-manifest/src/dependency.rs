use crate::{Dependency, DependencySource, SourceSpec};
use std::path::Path;
use toml::Value;
use tong_core::error::{Result, TongError};
use tong_core::paths;

use super::parser::{bool_value, string_array_value, string_value};

pub(super) fn parse_dependency(
    manifest: &Path,
    root: &Path,
    alias: String,
    value: &Value,
) -> Result<Dependency> {
    let table;
    let values = match value {
        Value::String(version) => {
            table =
                toml::Table::from_iter([("version".to_owned(), Value::String(version.clone()))]);
            &table
        }
        Value::Table(values) => values,
        other => {
            table = toml::Table::from_iter([("version".to_owned(), other.clone())]);
            &table
        }
    };

    validate_dependency_keys(manifest, &alias, values)?;

    let package = string_value(values, "package").unwrap_or_else(|| alias.clone());
    let features = string_array_value(values, "features").unwrap_or_default();
    let default_features = bool_value(values, "default-features").unwrap_or(true);
    let optional = bool_value(values, "optional").unwrap_or(false);

    let source = if let Some(path) = string_value(values, "path") {
        DependencySource::Path(root.join(path))
    } else if values.contains_key("git") {
        DependencySource::Source(SourceSpec::Git {
            url: required_source_url(manifest, root, values, "git")?,
            rev: string_value(values, "rev")
                .or_else(|| string_value(values, "tag"))
                .or_else(|| string_value(values, "branch")),
            subdir: string_value(values, "subdir"),
        })
    } else if values.contains_key("tar") {
        DependencySource::Source(SourceSpec::Tar {
            url: required_source_url(manifest, root, values, "tar")?,
            sha256: string_value(values, "sha256"),
            strip_prefix: string_value(values, "strip-prefix"),
            subdir: string_value(values, "subdir"),
        })
    } else if values.contains_key("zip") {
        DependencySource::Source(SourceSpec::Zip {
            url: required_source_url(manifest, root, values, "zip")?,
            sha256: string_value(values, "sha256"),
            strip_prefix: string_value(values, "strip-prefix"),
            subdir: string_value(values, "subdir"),
        })
    } else if let Some(version) = string_value(values, "version") {
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

pub(super) fn parse_source_spec(
    manifest: &Path,
    root: &Path,
    name: String,
    values: &toml::Table,
) -> Result<(String, SourceSpec)> {
    let spec = if values.contains_key("git") {
        SourceSpec::Git {
            url: required_source_url(manifest, root, values, "git")?,
            rev: string_value(values, "rev")
                .or_else(|| string_value(values, "tag"))
                .or_else(|| string_value(values, "branch")),
            subdir: string_value(values, "subdir"),
        }
    } else if values.contains_key("tar") {
        SourceSpec::Tar {
            url: required_source_url(manifest, root, values, "tar")?,
            sha256: string_value(values, "sha256"),
            strip_prefix: string_value(values, "strip-prefix"),
            subdir: string_value(values, "subdir"),
        }
    } else if values.contains_key("zip") {
        SourceSpec::Zip {
            url: required_source_url(manifest, root, values, "zip")?,
            sha256: string_value(values, "sha256"),
            strip_prefix: string_value(values, "strip-prefix"),
            subdir: string_value(values, "subdir"),
        }
    } else {
        return Err(TongError::invalid_manifest(
            manifest.to_path_buf(),
            format!("tong source `{name}` needs git, tar, or zip"),
        ));
    };

    Ok((name, spec))
}

fn validate_dependency_keys(path: &Path, alias: &str, values: &toml::Table) -> Result<()> {
    for key in values.keys() {
        if matches!(
            key.as_str(),
            "version"
                | "path"
                | "git"
                | "tar"
                | "zip"
                | "package"
                | "features"
                | "default-features"
                | "optional"
                | "rev"
                | "tag"
                | "branch"
                | "sha256"
                | "strip-prefix"
                | "subdir"
        ) {
            continue;
        }
        let suggestion = match key.as_str() {
            "gitt" => " (did you mean `git`?)",
            "verison" => " (did you mean `version`?)",
            _ => "",
        };
        return Err(TongError::invalid_manifest(
            path.to_path_buf(),
            format!("[dependencies.{alias}] unknown key `{key}`{suggestion}"),
        ));
    }
    Ok(())
}

fn required_source_url(
    path: &Path,
    root: &Path,
    values: &toml::Table,
    key: &str,
) -> Result<String> {
    let value = string_value(values, key).ok_or_else(|| {
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
