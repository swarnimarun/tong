use crate::error::{IoContext, Result, TongError};
use crate::graph::ProjectGraph;
use crate::language::{BuildProfile, BuildRequest, LanguageBackend};
use crate::manifest::Manifest;
use crate::paths;
use crate::rust_backend::RustBackend;
use std::fs;
use std::path::{Path, PathBuf};

pub fn run(args: impl Iterator<Item = String>) -> Result<()> {
    let args: Vec<String> = args.collect();
    let command = args.first().map(String::as_str).unwrap_or("help");
    match command {
        "add" => add(&args[1..]),
        "build" => build(&args[1..]),
        "fetch" => fetch(&args[1..]),
        "plan" => plan(&args[1..]),
        "clean" => clean(&args[1..]),
        "help" | "-h" | "--help" => {
            print_help();
            Ok(())
        }
        "version" | "--version" | "-V" => {
            println!("tong {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        other => Err(TongError::unsupported(format!(
            "unknown command `{other}`; run `tong help`"
        ))),
    }
}

fn add(args: &[String]) -> Result<()> {
    let options = AddCliOptions::parse(args)?;
    let manifest_path = Manifest::discover(&options.path)?;
    let mut raw = fs::read_to_string(&manifest_path)
        .with_context(format!("failed to read {}", manifest_path.display()))?;
    let entry = options.dependency_entry();
    insert_dependency(&mut raw, &entry);
    fs::write(&manifest_path, raw)
        .with_context(format!("failed to write {}", manifest_path.display()))?;
    println!("added {} to {}", options.name, manifest_path.display());
    Ok(())
}

fn build(args: &[String]) -> Result<()> {
    let options = BuildCliOptions::parse(args)?;
    let manifest_path = Manifest::discover(&options.path)?;
    let manifest = Manifest::load(&manifest_path)?;
    let out_dir = manifest.root.join("target/tong");
    let graph = ProjectGraph::load(&manifest_path)?;

    let request = BuildRequest {
        manifest_path,
        out_dir,
        profile: options.profile,
        verbose: options.verbose,
    };

    let mut backend = RustBackend::new()?;
    if options.verbose {
        eprintln!("backend {}", backend.name());
    }
    let output = backend.build(&graph, &request)?;
    for artifact in output.artifacts {
        println!("{}", artifact.display());
    }
    Ok(())
}

fn fetch(args: &[String]) -> Result<()> {
    let options = BuildCliOptions::parse(args)?;
    let manifest_path = Manifest::discover(&options.path)?;
    let graph = ProjectGraph::load(&manifest_path)?;
    println!("fetched/resolved {} package(s)", graph.packages.len());
    Ok(())
}

fn plan(args: &[String]) -> Result<()> {
    let options = BuildCliOptions::parse(args)?;
    let manifest_path = Manifest::discover(&options.path)?;
    let graph = ProjectGraph::load(&manifest_path)?;
    println!("root = {}", manifest_path.display());
    for (key, node) in &graph.packages {
        println!(
            "package {} {} ({})",
            node.manifest.package.name,
            key.0.display(),
            node.manifest.kind.as_str()
        );
        if let Some(lib) = &node.manifest.lib {
            if lib.proc_macro {
                println!("  proc-macro {} {}", lib.name, lib.path.display());
            } else {
                println!("  lib {} {}", lib.name, lib.path.display());
            }
        }
        if let Some(build_script) = &node.manifest.build_script {
            println!("  build-script {}", build_script.display());
        }
        for bin in &node.manifest.bins {
            println!("  bin {} {}", bin.name, bin.path.display());
        }
        for dependency in &node.dependencies {
            println!("  dep {} {}", dependency.alias, dependency.key.0.display());
        }
    }
    Ok(())
}

fn clean(args: &[String]) -> Result<()> {
    let options = BuildCliOptions::parse(args)?;
    let manifest_path = Manifest::discover(&options.path)?;
    let manifest = Manifest::load(&manifest_path)?;
    let out_dir = manifest.root.join("target/tong");
    if out_dir.exists() {
        fs::remove_dir_all(&out_dir)
            .with_context(format!("failed to remove {}", out_dir.display()))?;
    }
    println!("removed {}", out_dir.display());
    Ok(())
}

#[derive(Debug, Clone)]
struct AddCliOptions {
    path: PathBuf,
    name: String,
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
    fn parse(args: &[String]) -> Result<Self> {
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

    fn dependency_entry(&self) -> String {
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

fn insert_dependency(raw: &mut String, entry: &str) {
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
struct BuildCliOptions {
    path: PathBuf,
    profile: BuildProfile,
    verbose: bool,
}

impl BuildCliOptions {
    fn parse(args: &[String]) -> Result<Self> {
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
        {
            path = paths::canonicalize(&path)?;
        } else if Path::new(&path).exists() {
            path = paths::canonicalize(&path)?;
        }

        Ok(Self {
            path,
            profile,
            verbose,
        })
    }
}

fn print_help() {
    println!(
        "\
tong {}

USAGE:
  tong add NAME SOURCE [OPTIONS]
  tong build [OPTIONS] [PATH]
  tong fetch [OPTIONS] [PATH]
  tong plan [OPTIONS] [PATH]
  tong clean [PATH]

COMMANDS:
  add       Add a git, tar, or zip dependency source to the selected manifest
  build     Build a Rust package from Tong.toml or Cargo.toml
  fetch     Resolve and materialize dependency sources
  plan      Print the packages and targets Tong discovered
  clean     Remove target/tong for the selected package
  help      Print this help
  version   Print the version

OPTIONS:
  --manifest-path PATH  Use an explicit Tong.toml or Cargo.toml
  --git URL             Add a git dependency source
  --tar URL             Add a tar/.crate dependency source
  --zip URL             Add a zip dependency source
  --features A,B        Enable dependency features when used with `tong add`
  --no-default-features Disable dependency default features when used with `tong add`
  --release             Build with release rustc flags
  --debug               Build with debug rustc flags
  -v, --verbose         Print action execution details
",
        env!("CARGO_PKG_VERSION")
    );
}
