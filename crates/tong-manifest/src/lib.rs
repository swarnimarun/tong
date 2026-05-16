use std::fs;
use std::path::{Path, PathBuf};
use tong_core::error::{IoContext, Result, TongError};
use tong_core::paths;

mod parser;
mod types;

pub use types::{
    BinTarget, Dependency, DependencySource, ExampleTarget, LibTarget, Manifest, ManifestKind,
    Package, SourceSpec, TestTarget, WorkspaceMetadata,
};

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
            && let Some(extends) = parser::parse_tong_extends(&path, &raw)?
        {
            let base = path
                .parent()
                .ok_or_else(|| {
                    TongError::invalid_manifest(path.clone(), "manifest has no parent directory")
                })?
                .join(extends);
            let mut base_manifest = Self::load(&base)?;
            // Overlay Tong.toml fields onto base manifest
            let tong_manifest = parser::parse_tong_overlay(&path, &raw)?;
            base_manifest.kind = ManifestKind::Tong;
            base_manifest.path = tong_manifest.path;
            base_manifest.root = tong_manifest.root;
            // Merge sources: Tong.toml sources override/add to base
            for (name, spec) in tong_manifest.sources {
                base_manifest.sources.insert(name, spec);
            }
            // Merge features: Tong.toml features override/add to base
            for (name, features) in tong_manifest.features {
                base_manifest.features.insert(name, features);
            }
            // Merge dependencies: Tong.toml deps override/add to base
            for dep in tong_manifest.dependencies {
                base_manifest.dependencies.retain(|d| d.alias != dep.alias);
                base_manifest.dependencies.push(dep);
            }
            return Ok(base_manifest);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_manifest_from_directory() {
        let root = std::env::temp_dir().join(format!(
            "tong-discover-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"demo\"\n").unwrap();

        let found = Manifest::discover(&root).unwrap();
        assert_eq!(found.file_name().unwrap(), "Cargo.toml");

        std::fs::remove_dir_all(root).unwrap();
    }
}
