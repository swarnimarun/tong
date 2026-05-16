use std::fs;
use std::path::{Path, PathBuf};
use tong_core::error::{IoContext, Result, TongError};
use tong_core::paths;

mod dependency;
mod parser;
mod types;

pub use types::{
    BinTarget, Dependency, DependencySource, ExampleTarget, LibTarget, Manifest, ManifestKind,
    Package, SourceSpec, TestTarget, TongConfig, Workspace,
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
        let raw = fs::read_to_string(&path)
            .with_context(format!("failed to read manifest {}", path.display()))?;

        if matches!(kind, ManifestKind::Tong)
            && let Some(extends) = parser::parse_tong_extends(&raw, &path)?
        {
            let base = path
                .parent()
                .ok_or_else(|| {
                    TongError::invalid_manifest(path.clone(), "manifest has no parent directory")
                })?
                .join(&extends);
            let mut base_manifest = Self::load(&base)?;
            base_manifest.path = path.clone();
            base_manifest.kind = kind;

            let tong_table: toml::Table = toml::from_str(&raw).map_err(|e| {
                TongError::invalid_manifest(path.clone(), format!("toml parse error: {e}"))
            })?;

            if let Some(tong) = tong_table.get("tong").and_then(|v| v.as_table())
                && let Some(sandbox) = tong.get("sandbox").and_then(|v| v.as_str())
            {
                base_manifest.tong = Some(TongConfig {
                    sandbox: Some(sandbox.to_owned()),
                    extends: base_manifest.tong.as_ref().and_then(|t| t.extends.clone()),
                });
            }

            return Ok(base_manifest);
        }

        parser::parse_manifest_toml(path, kind, &raw)
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
