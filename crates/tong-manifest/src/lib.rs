use std::path::{Path, PathBuf};
use tong_core::error::{IoContext, Result, TongError};
use tong_core::paths;

mod dependency;
mod parser;
mod types;

pub use types::{
    BinTarget, Dependency, DependencySource, ExampleTarget, LibTarget, Manifest, ManifestKind,
    Package, SourceSpec, TestTarget, WorkspaceMeta,
};

#[cfg(test)]
mod tests;

impl Manifest {
    pub fn discover(start: &Path) -> Result<PathBuf> {
        let start = if start.as_os_str().is_empty() {
            Path::new(".")
        } else {
            start
        };

        if start.is_file() {
            return paths::canonicalize(start);
        }

        let start = paths::canonicalize(start)?;
        let mut current = Some(start.as_path());
        while let Some(dir) = current {
            let tong = dir.join("Tong.toml");
            if tong.exists() {
                return paths::canonicalize(&tong);
            }

            let cargo = dir.join("Cargo.toml");
            if cargo.exists() {
                return paths::canonicalize(&cargo);
            }

            current = dir.parent();
        }

        Err(TongError::unsupported(format!(
            "no Tong.toml or Cargo.toml found from {}",
            start.display()
        )))
    }

    pub fn load(path: &Path) -> Result<Self> {
        let path = paths::canonicalize(path)?;
        let kind = manifest_kind(&path)?;
        let raw = std::fs::read_to_string(&path)
            .with_context(format!("failed to read manifest {}", path.display()))?;

        if matches!(kind, ManifestKind::Tong)
            && let Some(extends) = parse_tong_extends(&raw)?
        {
            let base = path
                .parent()
                .ok_or_else(|| {
                    TongError::invalid_manifest(path.clone(), "manifest has no parent directory")
                })?
                .join(extends);
            return load_with_extends(&path, &base);
        }

        parser::parse_manifest(path, kind, &raw)
    }
}

impl ManifestKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cargo => "Cargo.toml",
            Self::Tong => "Tong.toml",
        }
    }
}

fn manifest_kind(path: &Path) -> Result<ManifestKind> {
    match path.file_name().and_then(|name| name.to_str()) {
        Some("Cargo.toml") => Ok(ManifestKind::Cargo),
        Some("Tong.toml") => Ok(ManifestKind::Tong),
        _ => Err(TongError::invalid_manifest(
            path.to_path_buf(),
            "manifest must be named Cargo.toml or Tong.toml",
        )),
    }
}

fn parse_tong_extends(raw: &str) -> Result<Option<String>> {
    let doc: toml::Value = raw
        .parse()
        .map_err(|e: toml::de::Error| TongError::parse(PathBuf::from("<raw>"), 0, e.message()))?;

    if let Some(toml::Value::String(extends)) = doc.get("extends") {
        return Ok(Some(extends.clone()));
    }
    if let Some(tong) = doc.get("tong").and_then(|v| v.as_table())
        && let Some(toml::Value::String(extends)) = tong.get("extends")
    {
        return Ok(Some(extends.clone()));
    }
    Ok(None)
}

fn load_with_extends(tong_path: &Path, base_path: &Path) -> Result<Manifest> {
    let base_manifest = {
        let base_path = paths::canonicalize(base_path)?;
        let base_raw = std::fs::read_to_string(&base_path).with_context(format!(
            "failed to read base manifest {}",
            base_path.display()
        ))?;
        parser::parse_manifest(base_path, ManifestKind::Cargo, &base_raw)?
    };

    let tong_path = paths::canonicalize(tong_path)?;
    let tong_raw = std::fs::read_to_string(&tong_path).with_context(format!(
        "failed to read Tong manifest {}",
        tong_path.display()
    ))?;
    let tong_manifest = parser::parse_manifest_with_name_required(
        tong_path.clone(),
        ManifestKind::Tong,
        &tong_raw,
        false,
    )?;

    Ok(merge_manifests(base_manifest, tong_manifest))
}

fn merge_manifests(base: Manifest, tong: Manifest) -> Manifest {
    let mut merged = base;

    if !tong.package.name.is_empty() {
        merged.package.name = tong.package.name;
    }
    if tong.package.version != "0.0.0" {
        merged.package.version = tong.package.version;
    }
    if tong.package.edition != "2021" {
        merged.package.edition = tong.package.edition;
    }

    if tong.lib.is_some() {
        merged.lib = tong.lib;
    }
    if !tong.bins.is_empty() {
        merged.bins = tong.bins;
    }
    if !tong.tests.is_empty() {
        merged.tests = tong.tests;
    }
    if !tong.examples.is_empty() {
        merged.examples = tong.examples;
    }
    if tong.build_script.is_some() {
        merged.build_script = tong.build_script;
    }

    for (key, value) in tong.features {
        merged.features.insert(key, value);
    }
    for (key, value) in tong.sources {
        merged.sources.insert(key, value);
    }
    for dep in tong.dependencies {
        if !merged.dependencies.iter().any(|d| d.alias == dep.alias) {
            merged.dependencies.push(dep);
        }
    }
    for dep in tong.build_dependencies {
        if !merged
            .build_dependencies
            .iter()
            .any(|d| d.alias == dep.alias)
        {
            merged.build_dependencies.push(dep);
        }
    }
    if let Some(workspace) = tong.workspace {
        merged.workspace = Some(workspace);
    }

    merged.path = tong.path;
    merged.root = tong.root;
    merged.kind = ManifestKind::Tong;

    merged
}
