use clap::{Parser, Subcommand};
use std::fs;
use std::io::{self, Read};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "cli-mini")]
#[command(about = "A tiny Tong-built CLI using clap derive")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Echo {
        text: Vec<String>,
    },
    Cat {
        path: Option<PathBuf>,
    },
}

fn main() -> io::Result<()> {
    let _git_source_dependency = git_flavor::source_kind();
    let _zip_source_dependency = zip_flavor::source_kind();
    let cli = Cli::parse();
    match cli.command {
        Command::Echo { text } => {
            println!("{}", text.join(" "));
        }
        Command::Cat { path } => {
            let mut input = String::new();
            if let Some(path) = path {
                input = fs::read_to_string(path)?;
            } else {
                io::stdin().read_to_string(&mut input)?;
            }
            print!("{input}");
        }
    }
    Ok(())
}
