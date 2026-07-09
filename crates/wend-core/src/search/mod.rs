//! Search: compile user input into a safe FTS5 query and run it.
//!
//! User text must never reach `MATCH` raw — quotes, `-`, `:`, `*`, `()` and the
//! `AND/OR/NOT` keywords would error or silently change semantics. We quote each
//! whitespace-separated term (doubling embedded quotes), which ANDs the terms and
//! treats every token as a literal.

use crate::error::Result;
use crate::store::{SearchHit, Store};

/// Compile free-text input into a safe FTS5 MATCH string. Returns `None` if the
/// input has no searchable terms.
pub fn compile_query(input: &str) -> Option<String> {
    let terms: Vec<String> = input
        .split_whitespace()
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect();
    if terms.is_empty() {
        None
    } else {
        Some(terms.join(" "))
    }
}

/// Run a keyword search, returning at most `limit` results — **one per session**
/// (the best-matching message). Returns empty for an empty query.
/// `role`, when set (`"user"` / `"assistant"`), restricts matches to messages of
/// that role. Titles have no role, so the title tier is skipped when it's set.
pub fn search(
    store: &Store,
    query: &str,
    limit: usize,
    role: Option<&str>,
) -> Result<Vec<SearchHit>> {
    let Some(match_query) = compile_query(query) else {
        return Ok(Vec::new());
    };
    // Tiered merge (corpus-size-independent): title/alias matches first — that's
    // a strong signal and `name`'s whole purpose — then message-body matches,
    // best-first. Dedup to one result per session. (A fixed additive bm25 boost
    // is fragile because body/title bm25 scales diverge as the corpus grows.)
    let mut seen = std::collections::HashSet::new();
    let mut grouped = Vec::with_capacity(limit);

    if role.is_none() {
        for hit in store.search_titles_raw(&match_query, limit)? {
            if seen.insert(hit.session_id.clone()) {
                grouped.push(hit);
                if grouped.len() >= limit {
                    return Ok(grouped);
                }
            }
        }
    }

    // Over-fetch body hits so a common term still yields enough distinct
    // sessions after dedup, but cap the raw pull. Must stay >= `limit` and never
    // let min>max (a `clamp(limit, CAP)` panics when limit>CAP).
    const RAW_CAP: usize = 50_000;
    let raw_limit = limit.saturating_mul(20).max(limit).min(RAW_CAP);
    for hit in store.search_raw(&match_query, raw_limit, role)? {
        if seen.insert(hit.session_id.clone()) {
            grouped.push(hit);
            if grouped.len() >= limit {
                break;
            }
        }
    }
    Ok(grouped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_each_term() {
        assert_eq!(compile_query("foo bar").as_deref(), Some("\"foo\" \"bar\""));
    }

    #[test]
    fn escapes_embedded_quotes_and_operators() {
        // a bare `-` or `OR` would be an FTS operator if unquoted; quoting neutralizes it.
        assert_eq!(
            compile_query("foo-bar OR baz").as_deref(),
            Some("\"foo-bar\" \"OR\" \"baz\"")
        );
        assert_eq!(
            compile_query("say \"hi\"").as_deref(),
            Some("\"say\" \"\"\"hi\"\"\"")
        );
    }

    #[test]
    fn empty_query_is_none() {
        assert_eq!(compile_query("   "), None);
    }
}
