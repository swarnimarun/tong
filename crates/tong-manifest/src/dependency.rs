use crate::{Dependency, DependencySource, SourceSpec};
use std::collections::BTreeMap;
use std::path::Path;
use tong_core::error::{Result, TongError};
use tong_core::paths;

pub(super) fn parse_dependency(
    manifest: &Path,
    root: &Path,
    alias: String,
    values: BTreeMap<String, toml::Value>,
) -> Result<Dependency> {
    let package = values
        .get("package")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .unwrap_or_else(|| alias.clone());
    let features = values
        .get("features")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let default_features = values
        .get("default-features")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let optional = values
        .get("optional")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let target = values
        .get("target")
        .and_then(|v| v.as_str())
        .map(str::to_owned);

    let source = if let Some(path) = values.get("path").and_then(|v| v.as_str()) {
        DependencySource::Path(root.join(path))
    } else if values.contains_key("git") {
        DependencySource::Source(SourceSpec::Git {
            url: required_source_url(manifest, root, &values, "git")?,
            rev: values
                .get("rev")
                .or_else(|| values.get("tag"))
                .or_else(|| values.get("branch"))
                .and_then(|v| v.as_str())
                .map(str::to_owned),
            subdir: values
                .get("subdir")
                .and_then(|v| v.as_str())
                .map(str::to_owned),
        })
    } else if values.contains_key("tar") {
        DependencySource::Source(SourceSpec::Tar {
            url: required_source_url(manifest, root, &values, "tar")?,
            sha256: values
                .get("sha256")
                .and_then(|v| v.as_str())
                .map(str::to_owned),
            strip_prefix: values
                .get("strip-prefix")
                .and_then(|v| v.as_str())
                .map(str::to_owned),
            subdir: values
                .get("subdir")
                .and_then(|v| v.as_str())
                .map(str::to_owned),
        })
    } else if values.contains_key("zip") {
        DependencySource::Source(SourceSpec::Zip {
            url: required_source_url(manifest, root, &values, "zip")?,
            sha256: values
                .get("sha256")
                .and_then(|v| v.as_str())
                .map(str::to_owned),
            strip_prefix: values
                .get("strip-prefix")
                .and_then(|v| v.as_str())
                .map(str::to_owned),
            subdir: values
                .get("subdir")
                .and_then(|v| v.as_str())
                .map(str::to_owned),
        })
    } else if let Some(version) = values.get("version").and_then(|v| v.as_str()) {
        DependencySource::Registry {
            version: version.to_owned(),
        }
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
        target,
    })
}

fn required_source_url(
    path: &Path,
    root: &Path,
    values: &BTreeMap<String, toml::Value>,
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
