use crate::error::{IoContext, Result, TongError};
use crate::hash::{StableHasher, hash_file};
use crate::manifest::SourceSpec;
use crate::paths;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone)]
pub struct SourceFetcher {
    root: PathBuf,
}

impl SourceFetcher {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn materialize(&self, name: &str, spec: &SourceSpec) -> Result<PathBuf> {
        fs::create_dir_all(&self.root).with_context(format!(
            "failed to create source store {}",
            self.root.display()
        ))?;

        match spec {
            SourceSpec::Git { subdir, .. } => self.materialize_git(name, spec, subdir.as_deref()),
            SourceSpec::Tar { subdir, .. } | SourceSpec::Zip { subdir, .. } => {
                self.materialize_archive(name, spec, subdir.as_deref())
            }
        }
    }

    fn materialize_git(
        &self,
        name: &str,
        spec: &SourceSpec,
        subdir: Option<&str>,
    ) -> Result<PathBuf> {
        let SourceSpec::Git { url, rev, .. } = spec else {
            unreachable!("materialize_git called with non-git source");
        };
        let source_root = self.source_root(name, spec);
        if !source_root.exists() {
            let git = paths::find_program("git")?;
            run(Command::new(&git)
                .arg("clone")
                .arg("--recurse-submodules")
                .arg(url)
                .arg(&source_root))?;
            if let Some(rev) = rev {
                run(Command::new(&git)
                    .arg("-C")
                    .arg(&source_root)
                    .arg("checkout")
                    .arg(rev))?;
            }
            fs::write(source_root.join(".tong-source"), source_key(name, spec))
                .with_context(format!("failed to stamp {}", source_root.display()))?;
        }
        Ok(resolve_subdir(source_root, subdir))
    }

    fn materialize_archive(
        &self,
        name: &str,
        spec: &SourceSpec,
        subdir: Option<&str>,
    ) -> Result<PathBuf> {
        let source_root = self.source_root(name, spec);
        if source_root.exists() {
            return Ok(resolve_subdir(source_root, subdir));
        }

        let download = self.download_archive(name, spec)?;
        verify_archive(spec, &download)?;

        let temp = self.root.join(format!(".tmp-{}", source_key(name, spec)));
        if temp.exists() {
            fs::remove_dir_all(&temp)
                .with_context(format!("failed to remove stale {}", temp.display()))?;
        }
        fs::create_dir_all(&temp).with_context(format!("failed to create {}", temp.display()))?;

        match spec {
            SourceSpec::Tar { .. } => {
                let tar = paths::find_program("tar")?;
                run(Command::new(&tar)
                    .arg("-xf")
                    .arg(&download)
                    .arg("-C")
                    .arg(&temp))?;
            }
            SourceSpec::Zip { .. } => {
                let unzip = paths::find_program("unzip")?;
                run(Command::new(&unzip)
                    .arg("-q")
                    .arg(&download)
                    .arg("-d")
                    .arg(&temp))?;
            }
            SourceSpec::Git { .. } => unreachable!("archive materializer called with git source"),
        }

        let extracted = archive_extracted_root(spec, &temp)?;
        if source_root.exists() {
            fs::remove_dir_all(&source_root)
                .with_context(format!("failed to remove stale {}", source_root.display()))?;
        }
        fs::rename(&extracted, &source_root).with_context(format!(
            "failed to move {} to {}",
            extracted.display(),
            source_root.display()
        ))?;
        if temp.exists() {
            fs::remove_dir_all(&temp)
                .with_context(format!("failed to remove temp {}", temp.display()))?;
        }
        fs::write(source_root.join(".tong-source"), source_key(name, spec))
            .with_context(format!("failed to stamp {}", source_root.display()))?;

        Ok(resolve_subdir(source_root, subdir))
    }

    fn download_archive(&self, name: &str, spec: &SourceSpec) -> Result<PathBuf> {
        let url = archive_url(spec);
        let downloads = self.root.join("downloads");
        fs::create_dir_all(&downloads)
            .with_context(format!("failed to create {}", downloads.display()))?;
        let file_name = url_file_name(url).unwrap_or_else(|| format!("{name}.archive"));
        let output = downloads.join(format!("{}-{file_name}", source_key(name, spec)));
        if output.exists() {
            return Ok(output);
        }

        if let Some(cache_path) = cargo_cache_archive(url) {
            fs::copy(&cache_path, &output).with_context(format!(
                "failed to copy {} to {}",
                cache_path.display(),
                output.display()
            ))?;
            return Ok(output);
        }

        if let Some(local) = local_url_path(url) {
            fs::copy(&local, &output).with_context(format!(
                "failed to copy {} to {}",
                local.display(),
                output.display()
            ))?;
            return Ok(output);
        }

        let curl = paths::find_program("curl")?;
        run(Command::new(&curl)
            .arg("-L")
            .arg("--fail")
            .arg("-o")
            .arg(&output)
            .arg(url))?;
        Ok(output)
    }

    fn source_root(&self, name: &str, spec: &SourceSpec) -> PathBuf {
        self.root.join(format!(
            "{}-{}",
            source_key(name, spec),
            sanitize_name(name)
        ))
    }
}

fn verify_archive(spec: &SourceSpec, path: &Path) -> Result<()> {
    let expected = match spec {
        SourceSpec::Tar { sha256, .. } | SourceSpec::Zip { sha256, .. } => sha256,
        SourceSpec::Git { .. } => return Ok(()),
    };
    let Some(expected) = expected else {
        return Ok(());
    };

    let actual = sha256_file(path)?;
    if actual != expected.to_lowercase() {
        return Err(TongError::unsupported(format!(
            "sha256 mismatch for {}: expected {}, got {}",
            path.display(),
            expected,
            actual
        )));
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String> {
    let shasum = paths::find_program("shasum");
    if let Ok(shasum) = shasum {
        let output = Command::new(shasum)
            .arg("-a")
            .arg("256")
            .arg(path)
            .output()
            .with_context(format!("failed to hash {}", path.display()))?;
        if output.status.success() {
            return Ok(String::from_utf8_lossy(&output.stdout)
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_lowercase());
        }
    }

    let sha256sum = paths::find_program("sha256sum")?;
    let output = Command::new(sha256sum)
        .arg(path)
        .output()
        .with_context(format!("failed to hash {}", path.display()))?;
    if !output.status.success() {
        return Err(TongError::CommandFailed {
            program: PathBuf::from("sha256sum"),
            status: output.status.to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_lowercase())
}

fn archive_extracted_root(spec: &SourceSpec, temp: &Path) -> Result<PathBuf> {
    let strip_prefix = match spec {
        SourceSpec::Tar { strip_prefix, .. } | SourceSpec::Zip { strip_prefix, .. } => {
            strip_prefix.as_deref()
        }
        SourceSpec::Git { .. } => None,
    };
    if let Some(strip_prefix) = strip_prefix {
        return Ok(temp.join(strip_prefix));
    }

    let mut entries = fs::read_dir(temp)
        .with_context(format!("failed to read {}", temp.display()))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(format!("failed to read entries in {}", temp.display()))?;
    entries.sort_by_key(|entry| entry.path());
    if entries.len() == 1 && entries[0].path().is_dir() {
        Ok(entries.remove(0).path())
    } else {
        Ok(temp.to_path_buf())
    }
}

fn run(command: &mut Command) -> Result<()> {
    let program = command.get_program().to_os_string();
    let output = command.output().with_context(format!(
        "failed to run {}",
        PathBuf::from(&program).display()
    ))?;
    if !output.status.success() {
        return Err(TongError::CommandFailed {
            program: PathBuf::from(program),
            status: output.status.to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(())
}

fn resolve_subdir(root: PathBuf, subdir: Option<&str>) -> PathBuf {
    subdir.map(|subdir| root.join(subdir)).unwrap_or(root)
}

fn archive_url(spec: &SourceSpec) -> &str {
    match spec {
        SourceSpec::Tar { url, .. } | SourceSpec::Zip { url, .. } => url,
        SourceSpec::Git { .. } => unreachable!("git source has no archive url"),
    }
}

fn source_key(name: &str, spec: &SourceSpec) -> String {
    let mut hasher = StableHasher::new();
    hasher.update_str("tong-source-v1");
    hasher.update_str(name);
    match spec {
        SourceSpec::Git { url, rev, subdir } => {
            hasher.update_str("git");
            hasher.update_str(url);
            if let Some(rev) = rev {
                hasher.update_str(rev);
            }
            if let Some(subdir) = subdir {
                hasher.update_str(subdir);
            }
        }
        SourceSpec::Tar {
            url,
            sha256,
            strip_prefix,
            subdir,
        } => {
            hasher.update_str("tar");
            hasher.update_str(url);
            if let Some(sha256) = sha256 {
                hasher.update_str(sha256);
            }
            if let Some(strip_prefix) = strip_prefix {
                hasher.update_str(strip_prefix);
            }
            if let Some(subdir) = subdir {
                hasher.update_str(subdir);
            }
        }
        SourceSpec::Zip {
            url,
            sha256,
            strip_prefix,
            subdir,
        } => {
            hasher.update_str("zip");
            hasher.update_str(url);
            if let Some(sha256) = sha256 {
                hasher.update_str(sha256);
            }
            if let Some(strip_prefix) = strip_prefix {
                hasher.update_str(strip_prefix);
            }
            if let Some(subdir) = subdir {
                hasher.update_str(subdir);
            }
        }
    }
    hasher.finish_hex()
}

fn sanitize_name(name: &str) -> String {
    name.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .collect()
}

fn url_file_name(url: &str) -> Option<String> {
    let without_query = url.split('?').next().unwrap_or(url);
    without_query
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
}

fn cargo_cache_archive(url: &str) -> Option<PathBuf> {
    let file_name = url_file_name(url)?;
    if !url.contains("static.crates.io/crates/") && !file_name.ends_with(".crate") {
        return None;
    }
    let home = std::env::var_os("HOME")?;
    let cache = PathBuf::from(home).join(".cargo/registry/cache");
    let registries = fs::read_dir(cache).ok()?;
    for registry in registries.flatten() {
        let candidate = registry.path().join(&file_name);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

fn local_url_path(url: &str) -> Option<PathBuf> {
    if let Some(path) = url.strip_prefix("file://") {
        return Some(PathBuf::from(path));
    }
    let path = PathBuf::from(url);
    path.exists().then_some(path)
}

#[allow(dead_code)]
fn _content_hash(path: &Path) -> Result<String> {
    hash_file(path)
}
