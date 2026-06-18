//! Path resolution for the index database and the Claude Code transcript dir.
//! Honors `WEND_DB` and `WEND_PROJECTS` env overrides (used by tests).

use crate::error::{Error, Result};
use etcetera::{choose_app_strategy, AppStrategy, AppStrategyArgs};
use std::path::PathBuf;

fn app_strategy() -> Result<impl AppStrategy> {
    choose_app_strategy(AppStrategyArgs {
        top_level_domain: "com".to_string(),
        author: "wend".to_string(),
        app_name: "wend".to_string(),
    })
    .map_err(|e| Error::InvalidData(format!("cannot resolve app directories: {e}")))
}

/// Path to the index database (`…/wend/index.db`). Override with `WEND_DB`.
pub fn index_db_path() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("WEND_DB") {
        return Ok(PathBuf::from(p));
    }
    Ok(app_strategy()?.data_dir().join("index.db"))
}

/// Directory for cached embedding models. Override with `WEND_MODEL_CACHE`.
pub fn model_cache_dir() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("WEND_MODEL_CACHE") {
        return Ok(PathBuf::from(p));
    }
    Ok(app_strategy()?.cache_dir().join("models"))
}

/// Directory holding Claude Code session transcripts (`~/.claude/projects`).
/// Override with `WEND_PROJECTS`.
pub fn projects_dir() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("WEND_PROJECTS") {
        return Ok(PathBuf::from(p));
    }
    let home = etcetera::home_dir()
        .map_err(|e| Error::InvalidData(format!("cannot find home dir: {e}")))?;
    Ok(home.join(".claude").join("projects"))
}
