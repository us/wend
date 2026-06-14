//! session-recall CLI entry point.

mod cli;

use anyhow::Result;
use clap::Parser;
use cli::{Cli, Command};

fn main() -> std::process::ExitCode {
    let args = Cli::parse();
    init_logging(args.verbose);

    match run(args) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            // Print the full context chain to stderr; stdout stays clean for results.
            eprintln!("error: {err:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run(args: Cli) -> Result<()> {
    match args.command {
        Command::Doctor => {
            println!("recall {}", recall_core::VERSION);
            println!("core: ok");
            Ok(())
        }
        other => {
            anyhow::bail!("command not yet implemented: {other:?}");
        }
    }
}

/// Initialize tracing to **stderr** (stdout is reserved for command output/JSON,
/// so the skill and shell pipes stay clean).
fn init_logging(verbose: u8) {
    use tracing_subscriber::{fmt, EnvFilter};

    let default_level = match verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    let filter =
        EnvFilter::try_from_env("RECALL_LOG").unwrap_or_else(|_| EnvFilter::new(default_level));

    fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();
}
