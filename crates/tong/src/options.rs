use std::path::{Path, PathBuf};
use tong_core::error::{Result, TongError};
use tong_core::language::BuildProfile;
use tong_core::paths;

#[derive(Debug, Clone)]
pub(super) struct AddCliOptions {
    pub(super) path: PathBuf,
    pub(super) name: String,
    source_kind: String,
    source: String,
    package: Option<String>,
    rev: Option<String>,
    sha256: Option<String>,
    strip_prefix: Option<String>,
    subdir: Option<String>,
    features: Vec<String>,
    default_features: Option<bool>,
}

impl AddCliOptions {
    pub(super) fn parse(args: &[String]) -> Result<Self> {
        let mut path = PathBuf::from(".");
        let mut name = None;
        let mut source = None;
        let mut source_kind = None;
        let mut package = None;
        let mut rev = None;
        let mut sha256 = None;
        let mut strip_prefix = None;
        let mut subdir = None;
        let mut features = Vec::new();
        let mut default_features = None;

        let mut iter = args.iter();
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--manifest-path" => {
                    let value = iter
                        .next()
                        .ok_or_else(|| TongError::unsupported("--manifest-path requires a path"))?;
                    path = PathBuf::from(value);
                }
                "--git" | "--tar" | "--zip" => {
                    let value = iter
                        .next()
                        .ok_or_else(|| TongError::unsupported(format!("{arg} requires a URL")))?;
                    source_kind = Some(arg.trim_start_matches("--").to_owned());
                    source = Some(value.clone());
                }
                "--package" => {
                    package = Some(next_value(&mut iter, "--package")?.clone());
                }
                "--rev" | "--tag" | "--branch" => {
                    rev = Some(next_value(&mut iter, arg)?.clone());
                }
                "--sha256" => {
                    sha256 = Some(next_value(&mut iter, "--sha256")?.clone());
                }
                "--strip-prefix" => {
                    strip_prefix = Some(next_value(&mut iter, "--strip-prefix")?.clone());
                }
                "--subdir" => {
                    subdir = Some(next_value(&mut iter, "--subdir")?.clone());
                }
                "--features" => {
                    features.extend(
                        next_value(&mut iter, "--features")?
                            .split(',')
                            .filter(|feature| !feature.trim().is_empty())
                            .map(|feature| feature.trim().to_owned()),
                    );
                }
                "--no-default-features" => default_features = Some(false),
                "--default-features" => default_features = Some(true),
                value if value.starts_with('-') => {
                    return Err(TongError::unsupported(format!("unknown flag `{value}`")));
                }
                value => {
                    if name.is_none() && source.is_none() && value.contains('=') {
                        let (dep_name, dep_source) = value
                            .split_once('=')
                            .ok_or_else(|| TongError::unsupported("invalid dependency-source"))?;
                        name = Some(dep_name.to_owned());
                        source = Some(dep_source.to_owned());
                    } else if name.is_none() {
                        name = Some(value.to_owned());
                    } else if source.is_none() {
                        source = Some(value.to_owned());
                    } else {
                        return Err(TongError::unsupported(format!(
                            "unexpected argument `{value}`"
                        )));
                    }
                }
            }
        }

        let name =
            name.ok_or_else(|| TongError::unsupported("tong add requires a dependency name"))?;
        let source = source
            .ok_or_else(|| TongError::unsupported("tong add requires a source"))?
            .trim_start_matches("git+")
            .to_owned();
        let source_kind = source_kind.unwrap_or_else(|| infer_source_kind(&source));

        Ok(Self {
            path,
            name,
            source_kind,
            source,
            package,
            rev,
            sha256,
            strip_prefix,
            subdir,
            features,
            default_features,
        })
    }

    pub(super) fn dependency_entry(&self) -> String {
        let mut fields = Vec::new();
        fields.push(format!("{} = {:?}", self.source_kind, self.source));
        if let Some(package) = &self.package {
            fields.push(format!("package = {package:?}"));
        }
        if let Some(rev) = &self.rev {
            fields.push(format!("rev = {rev:?}"));
        }
        if let Some(sha256) = &self.sha256 {
            fields.push(format!("sha256 = {sha256:?}"));
        }
        if let Some(strip_prefix) = &self.strip_prefix {
            fields.push(format!("strip-prefix = {strip_prefix:?}"));
        }
        if let Some(subdir) = &self.subdir {
            fields.push(format!("subdir = {subdir:?}"));
        }
        if !self.features.is_empty() {
            let features = self
                .features
                .iter()
                .map(|feature| format!("{feature:?}"))
                .collect::<Vec<_>>()
                .join(", ");
            fields.push(format!("features = [{features}]"));
        }
        if let Some(default_features) = self.default_features {
            fields.push(format!("default-features = {default_features}"));
        }
        format!("{} = {{ {} }}", self.name, fields.join(", "))
    }
}

pub(super) fn insert_dependency(raw: &mut String, entry: &str) {
    let mut lines = raw.lines().map(str::to_owned).collect::<Vec<_>>();
    let dependencies = lines
        .iter()
        .position(|line| line.trim() == "[dependencies]");
    match dependencies {
        Some(index) => {
            let insert_at = lines
                .iter()
                .enumerate()
                .skip(index + 1)
                .find_map(|(idx, line)| line.trim_start().starts_with('[').then_some(idx))
                .unwrap_or(lines.len());
            lines.insert(insert_at, entry.to_owned());
            *raw = lines.join("\n");
            raw.push('\n');
        }
        None => {
            if !raw.ends_with('\n') {
                raw.push('\n');
            }
            raw.push_str("\n[dependencies]\n");
            raw.push_str(entry);
            raw.push('\n');
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct BuildCliOptions {
    pub(super) path: PathBuf,
    pub(super) profile: BuildProfile,
    pub(super) verbose: bool,
}

impl BuildCliOptions {
    pub(super) fn parse(args: &[String]) -> Result<Self> {
        let mut path = PathBuf::from(".");
        let mut profile = BuildProfile::Debug;
        let mut verbose = false;

        let mut iter = args.iter();
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--release" => profile = BuildProfile::Release,
                "--debug" => profile = BuildProfile::Debug,
                "-v" | "--verbose" => verbose = true,
                "--manifest-path" => {
                    let value = iter
                        .next()
                        .ok_or_else(|| TongError::unsupported("--manifest-path requires a path"))?;
                    path = PathBuf::from(value);
                }
                value if value.starts_with('-') => {
                    return Err(TongError::unsupported(format!("unknown flag `{value}`")));
                }
                value => path = PathBuf::from(value),
            }
        }

        if path.as_os_str().is_empty() {
            path = PathBuf::from(".");
        }

        if path.file_name().and_then(|name| name.to_str()) == Some("Cargo.toml")
            || path.file_name().and_then(|name| name.to_str()) == Some("Tong.toml")
            || Path::new(&path).exists()
        {
            path = paths::canonicalize(&path)?;
        }

        Ok(Self {
            path,
            profile,
            verbose,
        })
    }
}

#[derive(Debug, Clone)]
pub(super) struct RunCliOptions {
    pub(super) build: BuildCliOptions,
    pub(super) bin: Option<String>,
    pub(super) args: Vec<String>,
}

impl RunCliOptions {
    pub(super) fn parse(args: &[String]) -> Result<Self> {
        let mut build_args = Vec::new();
        let mut run_args = Vec::new();
        let mut bin = None;
        let mut passthrough = false;

        let mut iter = args.iter();
        while let Some(arg) = iter.next() {
            if passthrough {
                run_args.push(arg.clone());
                continue;
            }

            match arg.as_str() {
                "--" => passthrough = true,
                "--bin" => {
                    let value = iter
                        .next()
                        .ok_or_else(|| TongError::unsupported("--bin requires a binary name"))?;
                    bin = Some(value.clone());
                }
                _ => build_args.push(arg.clone()),
            }
        }

        Ok(Self {
            build: BuildCliOptions::parse(&build_args)?,
            bin,
            args: run_args,
        })
    }
}

fn next_value<'a>(iter: &mut std::slice::Iter<'a, String>, flag: &str) -> Result<&'a String> {
    iter.next()
        .ok_or_else(|| TongError::unsupported(format!("{flag} requires a value")))
}

fn infer_source_kind(source: &str) -> String {
    let source = source.split('?').next().unwrap_or(source);
    if source.ends_with(".zip") {
        "zip".to_owned()
    } else if source.ends_with(".git") || source.starts_with("git+") {
        "git".to_owned()
    } else {
        "tar".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_build_options() {
        let options = BuildCliOptions::parse(&[
            "--release".to_owned(),
            "--verbose".to_owned(),
            "examples/simple-rust-project".to_owned(),
        ])
        .unwrap();

        assert_eq!(options.profile, BuildProfile::Release);
        assert!(options.verbose);
        assert!(options.path.ends_with("examples/simple-rust-project"));
    }

    #[test]
    fn formats_add_dependency_entry() {
        let options = AddCliOptions::parse(&[
            "clap".to_owned(),
            "--tar".to_owned(),
            "https://example.com/clap.crate".to_owned(),
            "--sha256".to_owned(),
            "abc".to_owned(),
            "--features".to_owned(),
            "derive,std".to_owned(),
            "--no-default-features".to_owned(),
        ])
        .unwrap();

        assert_eq!(
            options.dependency_entry(),
            r#"clap = { tar = "https://example.com/clap.crate", sha256 = "abc", features = ["derive", "std"], default-features = false }"#
        );
    }

    #[test]
    fn inserts_dependency_into_existing_section() {
        let mut raw =
            "[package]\nname = \"demo\"\n\n[dependencies]\na = \"1\"\n\n[features]\ndefault = []\n"
                .to_owned();
        insert_dependency(&mut raw, "b = \"2\"");

        let inserted = raw.find("b = \"2\"").unwrap();
        let features = raw.find("[features]").unwrap();
        assert!(inserted < features);
    }

    #[test]
    fn parses_run_options_with_bin_and_args() {
        let options = RunCliOptions::parse(&[
            "--release".to_owned(),
            "--bin".to_owned(),
            "hello-tong".to_owned(),
            "examples/simple-rust-project".to_owned(),
            "--".to_owned(),
            "one".to_owned(),
            "--flag".to_owned(),
        ])
        .unwrap();

        assert_eq!(options.build.profile, BuildProfile::Release);
        assert_eq!(options.bin.as_deref(), Some("hello-tong"));
        assert_eq!(options.args, ["one", "--flag"]);
        assert!(options.build.path.ends_with("examples/simple-rust-project"));
    }
}
