//! The `tong` command-line interface.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};

mod dockerfile;
mod driver;
mod manifest_mode;

use driver::{BuildOptions, BuildOutcome};

#[derive(Parser)]
#[command(
    name = "tong",
    version,
    about = "Tong hermetic multi-language build system"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// Build-target kinds (lib/bin/test/example/bench), for kind selectors.
use driver::{KIND_ALL, KIND_BENCH, KIND_BIN, KIND_EXAMPLE, KIND_LIB, KIND_TEST};

/// Cargo-style build selection and flags shared by `build`, `check`,
/// `run`, `test`, and `bench`.
#[derive(Args, Clone)]
struct BuildFlags {
    /// Profile name (dev or release by default).
    #[arg(long, default_value = "dev")]
    profile: String,
    /// Select workspace packages by spec: `name`, `name@version`, or
    /// `name@version#source`.
    #[arg(short = 'p', long = "package", value_delimiter = ',')]
    package: Vec<String>,
    /// `--deps-only` builds external dependencies and skips all workspace
    /// actions (docker layer staging).
    #[arg(long)]
    deps_only: bool,
    /// Build the library target.
    #[arg(long)]
    lib: bool,
    /// Build the binary targets.
    #[arg(long)]
    bins: bool,
    /// Build the example targets.
    #[arg(long)]
    examples: bool,
    /// Build the test targets (and run them for `tong test`).
    #[arg(long)]
    tests: bool,
    /// Build the benchmark targets (and run them for `tong bench`).
    #[arg(long)]
    benches: bool,
    /// Build every target kind.
    #[arg(long)]
    all_targets: bool,
    /// Rust target triple for target units (host by default).
    #[arg(long)]
    target: Option<String>,
    /// Features to activate on the selected packages.
    #[arg(long, value_delimiter = ',')]
    features: Vec<String>,
    /// Disable the selected packages' default feature.
    #[arg(long)]
    no_default_features: bool,
    /// Activate every declared feature of the selected packages.
    #[arg(long)]
    all_features: bool,
    /// Plan test/bench compiles without their run actions.
    #[arg(long)]
    no_run: bool,
    /// Never touch the network: missing locks, sources, or pinned
    /// toolchains fail with a targeted diagnostic instead of being
    /// fetched.
    #[arg(long)]
    offline: bool,
    /// Forbid rewriting `Tong.lock` (missing or outdated locks fail).
    #[arg(long)]
    locked: bool,
    /// `--locked` plus `--offline` (read-only, fully offline).
    #[arg(long)]
    frozen: bool,
}

impl BuildFlags {
    /// The selected target kinds for this flag set.
    fn kinds(&self, default: u32) -> u32 {
        if self.all_targets {
            KIND_ALL
        } else {
            let mut kinds = 0;
            if self.lib {
                kinds |= KIND_LIB;
            }
            if self.bins {
                kinds |= KIND_BIN;
            }
            if self.examples {
                kinds |= KIND_EXAMPLE;
            }
            if self.tests {
                kinds |= KIND_TEST;
            }
            if self.benches {
                kinds |= KIND_BENCH;
            }
            if kinds == 0 { default } else { kinds }
        }
    }

    fn options(&self, kinds: u32) -> BuildOptions {
        BuildOptions {
            profile: self.profile.clone(),
            targets: self.package.clone(),
            features: driver::FeatureOptions {
                features: self.features.clone(),
                no_default_features: self.no_default_features,
                all_features: self.all_features,
            },
            sandbox: None,
            deps_only: self.deps_only,
            offline: self.offline || self.frozen,
            locked: self.locked || self.frozen,
            check: false,
            no_run: self.no_run,
            kinds,
            target_triple: self.target.clone(),
        }
    }
}

#[derive(Subcommand)]
enum Command {
    /// Build the workspace or selected targets.
    Build {
        /// Native target labels (`:name`, `//member:name`); native mode
        /// only, and exclusive with `-p`.
        #[arg(value_name = "LABEL")]
        labels: Vec<String>,
        /// Build every workspace member.
        #[arg(long)]
        workspace: bool,
        #[command(flatten)]
        flags: BuildFlags,
    },
    /// Check (metadata-only) the workspace or selected targets.
    Check {
        #[arg(value_name = "LABEL")]
        labels: Vec<String>,
        #[arg(long)]
        workspace: bool,
        #[command(flatten)]
        flags: BuildFlags,
    },
    /// Build and run a binary target.
    Run {
        /// Target label (`:name`, `//member:name`) or `--bin <name>`.
        label: Option<String>,
        /// Run the named binary of the selected package.
        #[arg(long)]
        bin: Option<String>,
        /// Arguments passed to the program.
        #[arg(last = true)]
        args: Vec<String>,
        #[command(flatten)]
        flags: BuildFlags,
    },
    /// Remove the project-local `.tong` directory.
    Clean,
    /// Build and run the test targets.
    Test {
        /// Test label: a test name, a package name, or `pkg:name`.
        label: Option<String>,
        /// Run the named test target.
        #[arg(long)]
        test: Option<String>,
        /// Run the documentation tests.
        #[arg(long)]
        doc: bool,
        #[command(flatten)]
        flags: BuildFlags,
        /// Arguments passed to the test binaries.
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// Build and run the benchmark targets.
    Bench {
        /// Benchmark label or `--bench <name>`.
        label: Option<String>,
        /// Run the named benchmark target.
        #[arg(long)]
        bench: Option<String>,
        #[command(flatten)]
        flags: BuildFlags,
        /// Arguments passed to the benchmark binaries.
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// Query the target, dependency, or action graph.
    Query {
        /// What to query: `targets`, `deps`, or `actions`.
        what: String,
        /// Target label to scope the query (`deps`/`actions`).
        label: Option<String>,
        /// Output format.
        #[arg(long, default_value = "text")]
        format: String,
        #[command(flatten)]
        flags: BuildFlags,
    },
    /// Print the planned action graph.
    Graph {
        /// Output format: `json` or `dot`.
        #[arg(long, default_value = "json")]
        format: String,
        #[command(flatten)]
        flags: BuildFlags,
    },
    /// Explain why a target rebuilds, from the two newest build records.
    Explain {
        /// What to explain: `rebuild`.
        what: String,
        /// Target label.
        label: String,
        #[command(flatten)]
        flags: BuildFlags,
    },
    /// Show structured build events.
    Log {
        /// Output format.
        #[arg(long, default_value = "text")]
        format: String,
        #[command(flatten)]
        flags: BuildFlags,
    },
    /// Resolve versions and write `Tong.lock`.
    Lock {
        /// Use the cached index only; fail when an entry is missing.
        #[arg(long)]
        offline: bool,
    },
    /// Download locked crate sources into the store.
    Fetch {
        /// Never touch the network; fail when a source is missing.
        #[arg(long)]
        offline: bool,
    },
    /// Re-resolve `Tong.lock` (optionally for one package).
    Update {
        /// Only drop this package's lockfile preference.
        package: Option<String>,
    },
    /// Manage toolchains.
    Toolchain {
        #[command(subcommand)]
        command: ToolchainCommand,
    },
    /// Generate a layer-cache-friendly Dockerfile and .dockerignore.
    Dockerfile {
        /// Profile baked into the generated commands.
        #[arg(long, default_value = "dev")]
        profile: String,
        /// Toolchain-stage base image (required unless `[toolchain.rust]`
        /// version is pinned in Tong.toml).
        #[arg(long)]
        base: Option<String>,
        /// Runtime-stage base image.
        #[arg(long, default_value = "debian:bookworm-slim")]
        runtime_base: String,
        /// Directory to write Dockerfile and .dockerignore into.
        #[arg(long, default_value = ".")]
        output: PathBuf,
    },
    /// Garbage-collect the store: delete unreferenced cache objects.
    Gc {
        /// Delete unmarked objects older than this duration (`0` = all).
        #[arg(long)]
        older_than: Option<String>,
        /// Store size budget (e.g. `10G`, `500M`).
        #[arg(long)]
        max_size: Option<String>,
        /// Report what would be deleted without deleting.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
enum ToolchainCommand {
    /// Download a toolchain component bundle (rustup dist protocol).
    Fetch {
        /// Component name (`rust`).
        kind: String,
        /// Toolchain version, e.g. `1.90.0`.
        #[arg(long)]
        version: String,
        /// Target triple (defaults to the host).
        #[arg(long)]
        target: Option<String>,
    },
}

fn main() -> ExitCode {
    // Perf and metrics events go through tracing (target `tong::perf`,
    // controlled by `RUST_LOG`, written to stderr) so they can be forwarded
    // to a file or pipeline separately from the default stdout output.
    // Default level `warn`: nothing is emitted unless RUST_LOG opts in.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    match cli.command {
        Command::Build {
            labels,
            workspace: workspace_flag,
            flags,
        } => {
            let kinds = flags.kinds(KIND_LIB | KIND_BIN);
            let mut options = flags.options(kinds);
            options.targets.extend(labels);
            if workspace_flag {
                options.targets.clear();
            }
            match driver::build(&workspace, &options) {
                Ok(outcome) => {
                    print_summary(&outcome);
                    ExitCode::SUCCESS
                }
                Err(err) => {
                    eprintln!("tong: error: {err}");
                    ExitCode::FAILURE
                }
            }
        }
        Command::Check {
            labels,
            workspace: workspace_flag,
            flags,
        } => {
            let kinds = flags.kinds(KIND_LIB | KIND_BIN);
            let mut options = flags.options(kinds);
            options.check = true;
            options.targets.extend(labels);
            if workspace_flag {
                options.targets.clear();
            }
            match driver::build(&workspace, &options) {
                Ok(outcome) => {
                    print_summary(&outcome);
                    ExitCode::SUCCESS
                }
                Err(err) => {
                    eprintln!("tong: error: {err}");
                    ExitCode::FAILURE
                }
            }
        }
        Command::Run {
            label,
            bin,
            args,
            flags,
        } => {
            let kinds = flags.kinds(KIND_LIB | KIND_BIN);
            let mut options = flags.options(kinds);
            let label = label.or(bin);
            let Some(label) = label else {
                eprintln!("tong: error: `tong run` needs a target label or `--bin <name>`");
                return ExitCode::FAILURE;
            };
            options.targets.push(label.clone());
            match driver::run(&workspace, &label, &args, &options) {
                Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
                Err(err) => {
                    eprintln!("tong: error: {err}");
                    ExitCode::FAILURE
                }
            }
        }
        Command::Clean => match driver::clean(&workspace) {
            Ok(()) => {
                println!("cleaned .tong");
                ExitCode::SUCCESS
            }
            Err(err) => {
                eprintln!("tong: error: {err}");
                ExitCode::FAILURE
            }
        },
        Command::Test {
            label,
            test,
            doc,
            flags,
            args,
        } => {
            let kinds = flags.kinds(KIND_LIB | KIND_BIN | KIND_TEST | KIND_EXAMPLE);
            let options = flags.options(kinds);
            let label = label.or(test).or_else(|| flags.tests.then(String::new));
            let _ = doc;
            match driver::test(&workspace, label.as_deref(), &args, &options) {
                Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
                Err(err) => {
                    eprintln!("tong: error: {err}");
                    ExitCode::FAILURE
                }
            }
        }
        Command::Bench {
            label,
            bench,
            flags,
            args,
        } => {
            let kinds = flags.kinds(KIND_LIB | KIND_BIN | KIND_BENCH);
            let options = flags.options(kinds);
            let label = label.or(bench).or_else(|| flags.benches.then(String::new));
            match driver::bench(&workspace, label.as_deref(), &args, &options) {
                Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
                Err(err) => {
                    eprintln!("tong: error: {err}");
                    ExitCode::FAILURE
                }
            }
        }
        Command::Query {
            what,
            label,
            format,
            flags,
        } => {
            let options = flags.options(flags.kinds(KIND_ALL));
            match driver::query(&workspace, &what, label.as_deref(), &format, &options) {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => {
                    eprintln!("tong: error: {err}");
                    ExitCode::FAILURE
                }
            }
        }
        Command::Graph { format, flags } => {
            let options = flags.options(flags.kinds(KIND_ALL));
            match driver::graph(&workspace, &format, &options) {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => {
                    eprintln!("tong: error: {err}");
                    ExitCode::FAILURE
                }
            }
        }
        Command::Explain { what, label, flags } => {
            let options = flags.options(flags.kinds(KIND_ALL));
            match driver::explain(&workspace, &what, &label, &options) {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => {
                    eprintln!("tong: error: {err}");
                    ExitCode::FAILURE
                }
            }
        }
        Command::Log { format, flags } => {
            let options = flags.options(flags.kinds(KIND_ALL));
            match driver::log(&workspace, &format, &options) {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => {
                    eprintln!("tong: error: {err}");
                    ExitCode::FAILURE
                }
            }
        }
        Command::Lock { offline } => match driver::lock(&workspace, offline) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("tong: error: {err}");
                ExitCode::FAILURE
            }
        },
        Command::Fetch { offline } => match driver::fetch(&workspace, offline) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("tong: error: {err}");
                ExitCode::FAILURE
            }
        },
        Command::Update { package } => match driver::update(&workspace, package.as_deref()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("tong: error: {err}");
                ExitCode::FAILURE
            }
        },
        Command::Toolchain { command } => match command {
            ToolchainCommand::Fetch {
                kind,
                version,
                target,
            } => {
                if kind != "rust" {
                    eprintln!(
                        "tong: error: unsupported toolchain component {kind:?} (only \"rust\")"
                    );
                    return ExitCode::FAILURE;
                }
                match driver::toolchain_fetch(&workspace, &version, target.as_deref()) {
                    Ok(()) => ExitCode::SUCCESS,
                    Err(err) => {
                        eprintln!("tong: error: {err}");
                        ExitCode::FAILURE
                    }
                }
            }
        },
        Command::Gc {
            older_than,
            max_size,
            dry_run,
        } => {
            let opts = driver::GcCli {
                older_than,
                max_size,
                dry_run,
            };
            match driver::gc(&workspace, &opts) {
                Ok(_) => ExitCode::SUCCESS,
                Err(err) => {
                    eprintln!("tong: error: {err}");
                    ExitCode::FAILURE
                }
            }
        }
        Command::Dockerfile {
            profile,
            base,
            runtime_base,
            output,
        } => {
            let options = dockerfile::Options {
                profile,
                base,
                runtime_base,
                output: output.clone(),
            };
            match dockerfile::generate(&workspace, &options) {
                Ok(generated) => {
                    let dir = &options.output;
                    if let Err(err) = std::fs::create_dir_all(dir) {
                        eprintln!("tong: error: {}", err);
                        return ExitCode::FAILURE;
                    }
                    let dockerfile_path = dir.join("Dockerfile");
                    let dockerignore_path = dir.join(".dockerignore");
                    if let Err(err) = std::fs::write(&dockerfile_path, &generated.dockerfile) {
                        eprintln!("tong: error: {}", err);
                        return ExitCode::FAILURE;
                    }
                    if let Err(err) = std::fs::write(&dockerignore_path, &generated.dockerignore) {
                        eprintln!("tong: error: {}", err);
                        return ExitCode::FAILURE;
                    }
                    println!("wrote {}", dockerfile_path.display());
                    println!("wrote {}", dockerignore_path.display());
                    ExitCode::SUCCESS
                }
                Err(err) => {
                    eprintln!("tong: error: {err}");
                    ExitCode::FAILURE
                }
            }
        }
    }
}

fn print_summary(outcome: &BuildOutcome) {
    println!();
    if outcome.actions_skipped > 0 {
        println!(
            "build complete: {} actions ({} cached, {} executed, {} skipped)",
            outcome.actions_total,
            outcome.actions_cached,
            outcome.actions_executed,
            outcome.actions_skipped
        );
        println!("deps-only: workspace actions skipped, nothing assembled");
    } else {
        println!(
            "build complete: {} actions ({} cached, {} executed)",
            outcome.actions_total, outcome.actions_cached, outcome.actions_executed
        );
    }
    for artifact in &outcome.artifacts {
        println!("artifact: {}", artifact.display());
    }
}
