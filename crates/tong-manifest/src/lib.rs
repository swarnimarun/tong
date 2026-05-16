use std::fs;
use std::path::{Path, PathBuf};
use tong_core::error::{IoContext, Result, TongError};
use tong_core::paths;

mod dependency;
mod parser;
mod types;

pub use types::{
    BinTarget, Dependency, DependencySource, ExampleTarget, LibTarget, Manifest, ManifestKind,
    Package, SourceSpec, TestTarget, Workspace,
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
        let mut table = parse_toml_table(&path, &raw)?;

        if matches!(kind, ManifestKind::Tong)
            && let Some(extends) = parser::tong_extends(&table)
        {
            let base = path
                .parent()
                .ok_or_else(|| {
                    TongError::invalid_manifest(path.clone(), "manifest has no parent directory")
                })?
                .join(extends);
            let base = paths::canonicalize(&base)?;
            let base_raw = fs::read_to_string(&base)
                .with_context(format!("failed to read manifest {}", base.display()))?;
            let mut base_table = parse_toml_table(&base, &base_raw)?;
            shallow_merge(&mut base_table, table);
            table = base_table;
        }

        parser::parse_manifest(path, kind, table)
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

fn parse_toml_table(path: &Path, raw: &str) -> Result<toml::Table> {
    raw.parse::<toml::Table>().map_err(|err| {
        let line = err
            .span()
            .map(|span| line_for_offset(raw, span.start))
            .unwrap_or(1);
        TongError::parse(path.to_path_buf(), line, err.message().to_owned())
    })
}

fn line_for_offset(raw: &str, offset: usize) -> usize {
    raw[..offset.min(raw.len())]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count()
        + 1
}

fn shallow_merge(base: &mut toml::Table, overlay: toml::Table) {
    for (key, value) in overlay {
        base.insert(key, value);
    }
}
