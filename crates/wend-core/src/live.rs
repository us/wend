//! Live search: grep the raw transcripts without an index.
//!
//! The indexed path (`search`) needs `wend index` first and ranks with BM25.
//! This path needs nothing: it discovers the session files, parses them with
//! the same parser the indexer uses, and matches whitespace-separated terms
//! case-insensitively (all terms must appear, as in `search::compile_query`,
//! minus stemming; the indexed path ORs terms because bm25 ranks them, and this
//! path has no ranking). One hit per session, newest session first. Slower than
//! FTS on a huge corpus, instant to use.

use crate::error::Result;
use crate::index::{
    assemble, assemble_codex, discover_codex, discover_top_level, opencode, Sources,
};
use crate::model::MessageRecord;
use crate::parser::codex::parse_codex_file;
use crate::parser::parse_file;
use crate::store::FileStat;
use serde::Serialize;

/// One session that matched, with the most recent matching message as context.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LiveHit {
    pub source: String,
    pub session_id: String,
    pub title: String,
    pub project: String,
    /// Role of the matching message (`"user"` / `"assistant"`).
    pub role: String,
    /// One line of the matching message, capped.
    pub snippet: String,
    /// Epoch millis of the matching message, when known.
    pub ts: Option<i64>,
}

/// True when every term appears in `text` (case-insensitive).
pub fn match_terms(text: &str, terms: &[String]) -> bool {
    if terms.is_empty() {
        return false;
    }
    let lower = text.to_lowercase();
    terms.iter().all(|t| lower.contains(t.as_str()))
}

/// Split the query the way `search::compile_query` does, lowercased for
/// substring matching. Empty query → empty terms → no hits.
pub fn live_terms(query: &str) -> Vec<String> {
    query.split_whitespace().map(|t| t.to_lowercase()).collect()
}

/// The last message in `messages` that passes the role filter and matches all
/// terms, or `None`. Last match wins: it is the most recent context.
fn last_match<'a>(
    messages: &'a [MessageRecord],
    terms: &[String],
    role: Option<&str>,
) -> Option<&'a MessageRecord> {
    messages
        .iter()
        .rev()
        .filter(|m| {
            matches!(m.role.as_deref(), Some("user") | Some("assistant")) && !m.fts_text.is_empty()
        })
        .filter(|m| role.is_none_or(|r| m.role.as_deref() == Some(r)))
        .find(|m| match_terms(&m.fts_text, terms))
}

/// Scan one assembled session into a hit. `source`, `title`, `project` and
/// `session_id` come from the assembled session; the snippet is the last
/// matching message.
fn hit_for(
    source: &str,
    session_id: String,
    title: String,
    project: String,
    messages: &[MessageRecord],
    terms: &[String],
    role: Option<&str>,
) -> Option<LiveHit> {
    let m = last_match(messages, terms, role)?;
    Some(LiveHit {
        source: source.to_string(),
        session_id,
        title,
        project,
        role: m.role.clone().unwrap_or_default(),
        snippet: one_line(&m.fts_text, 200),
        ts: m.ts,
    })
}

fn dummy_stat() -> FileStat {
    FileStat {
        mtime_ns: 0,
        size: 0,
    }
}

/// Search the raw transcripts: Claude files, Codex rollout logs, and the
/// opencode database (read-only, no wend index involved). `role` (`"user"` /
/// `"assistant"`) and `source` (`"claude"` / `"codex"` / `"opencode"`)
/// restrict the scan. Returns at most `limit` hits, newest first.
pub fn search_live(
    sources: &Sources,
    query: &str,
    limit: usize,
    role: Option<&str>,
    source: Option<&str>,
) -> Result<Vec<LiveHit>> {
    use rayon::prelude::*;

    let terms = live_terms(query);
    if terms.is_empty() {
        return Ok(Vec::new());
    }
    let want = |name: &str| source.is_none_or(|s| s == name);

    let mut hits = Vec::new();

    if want("claude") {
        let mut claude: Vec<LiveHit> = discover_top_level(sources.claude)?
            .into_par_iter()
            .filter_map(|path| {
                let parsed = parse_file(&path).ok()?;
                let file_path = path.to_string_lossy().into_owned();
                let session_id = path.file_stem().map(|s| s.to_string_lossy().into_owned())?;
                let fallback = path
                    .parent()
                    .and_then(|p| p.file_name())
                    .map(|s| s.to_string_lossy().into_owned());
                let session = assemble(parsed, file_path, session_id, dummy_stat(), fallback);
                hit_for(
                    "claude",
                    session.session_id,
                    session.title,
                    session.project_name.unwrap_or_default(),
                    &session.messages,
                    &terms,
                    role,
                )
            })
            .collect();
        hits.append(&mut claude);
    }

    if want("codex") {
        let mut codex: Vec<LiveHit> = discover_codex(sources.codex)?
            .into_par_iter()
            .filter_map(|path| {
                let parsed = parse_codex_file(&path).ok()?;
                let file_path = path.to_string_lossy().into_owned();
                let fallback = path
                    .parent()
                    .and_then(|p| p.file_name())
                    .map(|s| s.to_string_lossy().into_owned());
                let session = assemble_codex(parsed, file_path, dummy_stat(), fallback);
                hit_for(
                    "codex",
                    session.session_id,
                    session.title,
                    session.project_name.unwrap_or_default(),
                    &session.messages,
                    &terms,
                    role,
                )
            })
            .collect();
        hits.append(&mut codex);
    }

    if want("opencode") {
        if let Some(db) = sources.opencode_db {
            for session in opencode::read_opencode_sessions(db)? {
                if let Some(hit) = hit_for(
                    "opencode",
                    session.session_id,
                    session.title,
                    session.project_name.unwrap_or_default(),
                    &session.messages,
                    &terms,
                    role,
                ) {
                    hits.push(hit);
                }
            }
        }
    }

    // Newest session first; hits without a timestamp sort last, stably.
    hits.sort_by_key(|a| std::cmp::Reverse(a.ts));
    hits.truncate(limit);
    Ok(hits)
}

fn one_line(s: &str, n: usize) -> String {
    let joined = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if joined.chars().count() <= n {
        joined
    } else {
        joined.chars().take(n).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terms_match_case_insensitively_and_conjunctively() {
        let terms = live_terms("Foo BAR");
        assert!(match_terms("foo and bar here", &terms));
        assert!(!match_terms("only foo here", &terms));
        assert!(!match_terms("nothing", &terms));
    }

    #[test]
    fn empty_query_matches_nothing() {
        assert!(live_terms("   ").is_empty());
        assert!(!match_terms("anything", &[]));
    }
}
