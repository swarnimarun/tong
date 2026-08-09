//! The `tong` command-line interface.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

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

#[derive(Subcommand)]
enum Command {
    /// Build the workspace.
    Build {
        /// Profile name (dev or release by default).
        #[arg(long, default_value = "dev")]
        profile: String,
        /// Restrict materialized artifacts to these targets.
        #[arg(long)]
        target: Vec<String>,
        /// Features to activate on the selected packages.
        #[arg(long, value_delimiter = ',')]
        features: Vec<String>,
        /// Disable the selected packages' default feature.
        #[arg(long)]
        no_default_features: bool,
        /// Activate every declared feature of the selected packages.
        #[arg(long)]
        all_features: bool,
    },
    /// Build and run a binary target.
    Run {
        /// Target label, e.g. `:hello`.
        target: String,
        /// Arguments passed to the program.
        #[arg(last = true)]
        args: Vec<String>,
        /// Profile name.
        #[arg(long, default_value = "dev")]
        profile: String,
        /// Features to activate on the selected packages.
        #[arg(long, value_delimiter = ',')]
        features: Vec<String>,
        /// Disable the selected packages' default feature.
        #[arg(long)]
        no_default_features: bool,
        /// Activate every declared feature of the selected packages.
        #[arg(long)]
        all_features: bool,
    },
    /// Remove the project-local `.tong` directory.
    Clean,
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

fn main() -> ExitCode {
    let cli = Cli::parse();
    let workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    match cli.command {
        Command::Build {
            profile,
            target,
            features,
            no_default_features,
            all_features,
        } => {
            let options = BuildOptions {
                profile,
                targets: target,
                features: driver::FeatureOptions {
                    features,
                    no_default_features,
                    all_features,
                },
            };
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
            target,
            args,
            profile,
            features,
            no_default_features,
            all_features,
        } => {
            let options = BuildOptions {
                profile,
                targets: vec![target.clone()],
                features: driver::FeatureOptions {
                    features,
                    no_default_features,
                    all_features,
                },
            };
            match driver::run(&workspace, &target, &args, &options) {
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
    }
}

fn print_summary(outcome: &BuildOutcome) {
    println!();
    println!(
        "build complete: {} actions ({} cached, {} executed)",
        outcome.actions_total, outcome.actions_cached, outcome.actions_executed
    );
    for artifact in &outcome.artifacts {
        println!("artifact: {}", artifact.display());
    }
}
