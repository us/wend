//! Command-line surface. Subcommands are scaffolded here and filled in across
//! the build steps (index → search → show → recover → tree → …).

use clap::{Parser, Subcommand};

/// wend: find, recover, and visualize your Claude Code session history.
#[derive(Debug, Parser)]
#[command(name = "wend", version, about)]
pub struct Cli {
    /// Increase log verbosity (-v, -vv). Logs go to stderr; stdout stays clean.
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Build or update the local index from ~/.claude/projects.
    Index {
        /// Re-index only files whose mtime/size changed.
        #[arg(long)]
        incremental: bool,
        /// Also index subagent transcripts (larger; topology-only by default).
        #[arg(long)]
        include_subagents: bool,
        /// Generate embeddings for semantic search (opt-in, slower).
        #[arg(long)]
        embed: bool,
    },
    /// Search past sessions by keyword.
    Search {
        /// Free-text query.
        query: String,
        /// Hybrid keyword + semantic search (not implemented yet; falls back to keyword).
        #[arg(long)]
        semantic: bool,
        /// Emit machine-readable JSON (for the skill).
        #[arg(long)]
        json: bool,
        /// Max results.
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Show a session transcript by id.
    Show {
        /// Session id (short UUID prefix accepted).
        id: String,
        /// Reconstruct and include pre-compaction history.
        #[arg(long)]
        recovered: bool,
        /// Show only the first N messages.
        #[arg(long)]
        head: Option<usize>,
        /// Show only the last N messages.
        #[arg(long)]
        tail: Option<usize>,
        /// Show a 1-based inclusive range, e.g. 10:20 (or 10-20).
        #[arg(long)]
        range: Option<String>,
        /// Print only the message count, nothing else.
        #[arg(long)]
        count: bool,
    },
    /// Show the worktree/subagent topology for a project.
    Tree {
        /// Project name or path filter (optional).
        project: Option<String>,
    },
    /// Print the command to resume a session.
    Resume {
        /// Session id (short UUID prefix accepted).
        id: String,
    },
    /// Export a session to a file.
    Export {
        /// Session id (short UUID prefix accepted).
        id: String,
        /// Output format.
        #[arg(long, default_value = "md")]
        format: String,
    },
    /// Give a session a custom alias (searchable).
    Name {
        /// Session id (short UUID prefix accepted).
        id: String,
        /// Alias text.
        alias: String,
    },
    /// Report index health, paths, and capabilities.
    Doctor,
}
