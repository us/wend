//! `recall-core` — the pure engine behind session-recall.
//!
//! Parses Claude Code `.jsonl` transcripts, indexes them, and serves search,
//! compaction-recovery, and worktree/subagent topology. This crate is UI-free
//! and never writes to stdout — that is the CLI's job.

pub mod config;
pub mod error;
pub mod index;
pub mod model;
pub mod parser;
pub mod search;
pub mod store;

pub use error::{Error, Result};

/// Crate version, surfaced by the CLI's `doctor` command.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_populated() {
        assert!(!VERSION.is_empty());
    }
}
