//! Typed error surface for the core engine. Libraries return `Error`; the CLI
//! wraps these with `anyhow` context.

/// Result alias used throughout `recall-core`.
pub type Result<T> = std::result::Result<T, Error>;

/// All failure modes the core engine can surface.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("invalid session data: {0}")]
    InvalidData(String),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("ambiguous id '{id}': {candidates} candidates match")]
    AmbiguousId { id: String, candidates: usize },
}
