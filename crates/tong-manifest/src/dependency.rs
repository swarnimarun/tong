use crate::{Dependency, DependencySource, SourceSpec};
use std::collections::BTreeMap;
use std::path::Path;
use tong_core::error::{Result, TongError};
use tong_core::paths;

use super::{TomlValue, optional_bool, optional_string, optional_string_array};

pub(super) fn parse_dependency(
    manifest: &Path,
    root: &Path,
    alias: String,
    values: BTreeMap<String, TomlValue>,
) -> Result<Dependency> {
    let package = optional_string(&values, "package").unwrap_or_else(|| alias.clone());
    let features = optional_string_array(&values, "features").unwrap_or_default();
    let default_features = optional_bool(&values, "default-features").unwrap_or(true);
    let optional = optional_bool(&values, "optional").unwrap_or(false);

    let source = if let Some(path) = optional_string(&values, "path") {
        DependencySource::Path(root.join(path))
    } else if values.contains_key("git") {
        DependencySource::Source(SourceSpec::Git {
            url: required_source_url(manifest, root, &values, "git")?,
            rev: optional_string(&values, "rev")
                .or_else(|| optional_string(&values, "tag"))
                .or_else(|| optional_string(&values, "branch")),
            subdir: optional_string(&values, "subdir"),
        })
    } else if values.contains_key("tar") {
        DependencySource::Source(SourceSpec::Tar {
            url: required_source_url(manifest, root, &values, "tar")?,
            sha256: optional_string(&values, "sha256"),
            strip_prefix: optional_string(&values, "strip-prefix"),
            subdir: optional_string(&values, "subdir"),
        })
    } else if values.contains_key("zip") {
        DependencySource::Source(SourceSpec::Zip {
            url: required_source_url(manifest, root, &values, "zip")?,
            sha256: optional_string(&values, "sha256"),
            strip_prefix: optional_string(&values, "strip-prefix"),
            subdir: optional_string(&values, "subdir"),
        })
    } else if let Some(version) = optional_string(&values, "version") {
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

pub(super) fn dependency_table(value: TomlValue) -> BTreeMap<String, TomlValue> {
    match value {
        TomlValue::InlineTable(table) => table,
        TomlValue::String(version) => {
            let mut table = BTreeMap::new();
            table.insert("version".to_owned(), TomlValue::String(version));
            table
        }
        other => {
            let mut table = BTreeMap::new();
            table.insert("version".to_owned(), other);
            table
        }
    }
}

pub(super) fn parse_source_spec(
    manifest: &Path,
    root: &Path,
    name: String,
    values: &BTreeMap<String, TomlValue>,
) -> Result<(String, SourceSpec)> {
    let spec = if values.contains_key("git") {
        SourceSpec::Git {
            url: required_source_url(manifest, root, values, "git")?,
            rev: optional_string(values, "rev")
                .or_else(|| optional_string(values, "tag"))
                .or_else(|| optional_string(values, "branch")),
            subdir: optional_string(values, "subdir"),
        }
    } else if values.contains_key("tar") {
        SourceSpec::Tar {
            url: required_source_url(manifest, root, values, "tar")?,
            sha256: optional_string(values, "sha256"),
            strip_prefix: optional_string(values, "strip-prefix"),
            subdir: optional_string(values, "subdir"),
        }
    } else if values.contains_key("zip") {
        SourceSpec::Zip {
            url: required_source_url(manifest, root, values, "zip")?,
            sha256: optional_string(values, "sha256"),
            strip_prefix: optional_string(values, "strip-prefix"),
            subdir: optional_string(values, "subdir"),
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
    values: &BTreeMap<String, TomlValue>,
    key: &str,
) -> Result<String> {
    let value = optional_string(values, key).ok_or_else(|| {
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
