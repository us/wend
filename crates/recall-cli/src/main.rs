//! session-recall CLI entry point.

mod cli;

use anyhow::{Context, Result};
use clap::Parser;
use cli::{Cli, Command};
use recall_core::{config, index, search, store::Store};

fn main() -> std::process::ExitCode {
    let args = Cli::parse();
    init_logging(args.verbose);

    match run(args) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run(args: Cli) -> Result<()> {
    match args.command {
        Command::Index {
            incremental,
            include_subagents,
            embed,
        } => {
            if include_subagents {
                tracing::warn!("--include-subagents not yet implemented; indexing top-level only");
            }
            if embed {
                tracing::warn!("--embed not yet implemented");
            }
            let db = config::index_db_path()?;
            let projects = config::projects_dir()?;
            let mut store =
                Store::open(&db).with_context(|| format!("opening index at {}", db.display()))?;
            let stats = index::index_all(&mut store, &projects, incremental)
                .with_context(|| format!("indexing {}", projects.display()))?;
            println!(
                "indexed {} session(s) — {} unchanged, {} files seen, {} bad lines skipped",
                stats.indexed, stats.skipped_unchanged, stats.files_seen, stats.parse_skipped_lines
            );
            println!("index: {}", db.display());
            Ok(())
        }

        Command::Search {
            query,
            semantic,
            json,
            limit,
        } => {
            if semantic {
                tracing::warn!("--semantic not yet implemented; keyword only");
            }
            let db = config::index_db_path()?;
            let store =
                Store::open(&db).with_context(|| format!("opening index at {}", db.display()))?;
            let hits = search::search(&store, &query, limit)?;
            if json {
                println!("{}", serde_json::to_string(&hits)?);
            } else if hits.is_empty() {
                println!("no matches for {query:?}");
            } else {
                for (i, h) in hits.iter().enumerate() {
                    let title = if h.title.is_empty() {
                        "(untitled)"
                    } else {
                        &h.title
                    };
                    println!("{}. [{}] {} · {}", i + 1, h.project, title, h.session_id);
                    println!("    {}", h.snippet);
                }
            }
            Ok(())
        }

        Command::Doctor => {
            let db = config::index_db_path()?;
            let projects = config::projects_dir()?;
            println!("recall {}", recall_core::VERSION);
            println!("projects dir: {}", projects.display());
            println!("index db:     {}", db.display());
            if db.exists() {
                let store = Store::open(&db)?;
                println!("sessions:     {}", store.session_count()?);
                println!("messages:     {}", store.message_count()?);
            } else {
                println!("index:        not built — run `recall index`");
            }
            Ok(())
        }

        other => anyhow::bail!("command not yet implemented: {other:?}"),
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
