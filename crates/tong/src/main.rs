mod cli;
mod options;

use tong_core::error::Result;

fn main() {
    if let Err(err) = run() {
        eprintln!("error: {err}");
        let mut source = std::error::Error::source(&err);
        while let Some(err) = source {
            eprintln!("  caused by: {err}");
            source = std::error::Error::source(err);
        }
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    cli::run(std::env::args().skip(1))
}
