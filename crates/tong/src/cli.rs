use crate::options::{AddCliOptions, BuildCliOptions, RunCliOptions, insert_dependency};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tong_core::error::{IoContext, Result, TongError};
use tong_core::language::{BuildRequest, LanguageBackend};
use tong_graph::ProjectGraph;
use tong_manifest::Manifest;
use tong_rust::RustBackend;

pub fn run(args: impl Iterator<Item = String>) -> Result<()> {
    let args: Vec<String> = args.collect();
    let command = args.first().map(String::as_str).unwrap_or("help");
    match command {
        "add" => add(&args[1..]),
        "build" => build(&args[1..]),
        "test" => test(&args[1..]),
        "run" => run_binary(&args[1..]),
        "fetch" => fetch(&args[1..]),
        "plan" => plan(&args[1..]),
        "clean" => clean(&args[1..]),
        "gc" => gc(&args[1..]),
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
    let output = build_project(&options)?;
    for artifact in output.artifacts {
        println!("{}", artifact.display());
    }
    Ok(())
}

fn test(args: &[String]) -> Result<()> {
    let mut args = args.to_vec();
    if !args.iter().any(|arg| arg == "--no-run") {
        args.push("--no-run".to_owned());
    }
    let mut options = BuildCliOptions::parse(&args)?;
    options.tests = true;
    let output = build_project(&options)?;
    for artifact in output.artifacts {
        println!("{}", artifact.display());
    }
    Ok(())
}

fn build_project(options: &BuildCliOptions) -> Result<tong_core::language::BuildOutput> {
    let manifest_path = Manifest::discover(&options.path)?;
    let manifest = Manifest::load(&manifest_path)?;
    let out_dir = manifest.root.join("target/tong");
    let graph = ProjectGraph::load(&manifest_path)?;
    tong_core::build_state::begin(&out_dir)?;
    record_materialized_sources(&out_dir, &graph)?;

    let request = BuildRequest {
        manifest_path,
        out_dir,
        profile: options.profile,
        verbose: options.verbose,
        build_examples: options.examples,
        build_tests: options.tests,
    };

    let mut backend = RustBackend::new()?;
    if options.verbose {
        eprintln!("backend {}", backend.name());
    }
    backend.build(&graph, &request)
}

fn record_materialized_sources(out_dir: &Path, graph: &ProjectGraph) -> Result<()> {
    let source_store = out_dir.join("store/sources");
    let roots = graph
        .packages
        .keys()
        .filter_map(|key| materialized_source_root(&source_store, &key.0))
        .collect::<Vec<_>>();
    if !roots.is_empty() {
        tong_core::build_state::record_paths(out_dir, roots.iter())?;
    }
    Ok(())
}

fn materialized_source_root(source_store: &Path, manifest_path: &Path) -> Option<PathBuf> {
    let relative = manifest_path.strip_prefix(source_store).ok()?;
    let first = relative.components().next()?;
    Some(source_store.join(first.as_os_str()))
}

fn run_binary(args: &[String]) -> Result<()> {
    let options = RunCliOptions::parse(args)?;
    let output = build_project(&options.build)?;
    let binary = select_binary(&output.artifacts, options.bin.as_deref())?;
    let status = Command::new(&binary)
        .args(&options.args)
        .status()
        .with_context(format!("failed to run {}", binary.display()))?;
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
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
        for dependency in &node.build_dependencies {
            println!(
                "  build-dep {} {}",
                dependency.alias,
                dependency.key.0.display()
            );
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

fn gc(args: &[String]) -> Result<()> {
    let options = BuildCliOptions::parse(args)?;
    let manifest_path = Manifest::discover(&options.path)?;
    let manifest = Manifest::load(&manifest_path)?;
    let out_dir = manifest.root.join("target/tong");
    let state = tong_core::build_state::BuildState::read(out_dir.clone()).map_err(|_| {
        TongError::unsupported(format!(
            "no usable build state at {}; run `tong build` first or use `tong clean` for a full reset",
            out_dir.join("build-state").display()
        ))
    })?;
    let removed = state.gc()?;
    println!(
        "removed {removed} stale artifact(s) from {}",
        out_dir.display()
    );
    Ok(())
}

fn select_binary(artifacts: &[PathBuf], name: Option<&str>) -> Result<PathBuf> {
    let binaries = artifacts
        .iter()
        .filter(|path| is_binary_artifact(path))
        .collect::<Vec<_>>();

    if let Some(name) = name {
        let executable = executable_name(name);
        return binaries
            .into_iter()
            .find(|path| path.file_name().and_then(|value| value.to_str()) == Some(&executable))
            .cloned()
            .ok_or_else(|| TongError::unsupported(format!("no built binary named `{name}`")));
    }

    match binaries.as_slice() {
        [binary] => Ok((*binary).clone()),
        [] => Err(TongError::unsupported(
            "selected package has no binary target",
        )),
        _ => Err(TongError::unsupported(
            "selected package has multiple binary targets; pass `--bin NAME`",
        )),
    }
}

fn is_binary_artifact(path: &Path) -> bool {
    path.parent()
        .and_then(|parent| parent.file_name())
        .and_then(|name| name.to_str())
        == Some("bin")
}

fn executable_name(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_owned()
    }
}

fn print_help() {
    println!(
        "\
tong {}

USAGE:
  tong add NAME SOURCE [OPTIONS]
  tong build [OPTIONS] [PATH]
  tong test --no-run [OPTIONS] [PATH]
  tong run [OPTIONS] [PATH] [-- ARGS...]
  tong fetch [OPTIONS] [PATH]
  tong plan [OPTIONS] [PATH]
  tong clean [PATH]
  tong gc [PATH]

COMMANDS:
  add       Add a git, tar, or zip dependency source to the selected manifest
  build     Build a Rust package from Tong.toml or Cargo.toml
  test      Compile test targets without running them
  run       Build and run a binary target
  fetch     Resolve and materialize dependency sources
  plan      Print the packages and targets Tong discovered
  clean     Remove target/tong for the selected package
  gc        Remove stale files under target/tong using the latest build-state
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
  --examples            Build example targets when used with `tong build`
  --no-run              Compile tests without running them when used with `tong test`
  --bin NAME            Select a binary target when used with `tong run`
  -v, --verbose         Print action execution details
",
        env!("CARGO_PKG_VERSION")
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_single_binary_artifact() {
        let binary = PathBuf::from("target/tong/debug/bin/demo");
        let artifacts = vec![
            PathBuf::from("target/tong/debug/deps/libdemo.rlib"),
            binary.clone(),
        ];

        assert_eq!(select_binary(&artifacts, None).unwrap(), binary);
    }

    #[test]
    fn requires_bin_name_for_multiple_binary_artifacts() {
        let one = executable_name("one");
        let two = executable_name("two");
        let artifacts = vec![
            PathBuf::from("target/tong/debug/bin").join(&one),
            PathBuf::from("target/tong/debug/bin").join(&two),
        ];

        assert!(select_binary(&artifacts, None).is_err());
        assert_eq!(
            select_binary(&artifacts, Some("two")).unwrap(),
            PathBuf::from("target/tong/debug/bin").join(two)
        );
    }
}
