//! Search: compile user input into a safe FTS5 query and run it.
//!
//! User text must never reach `MATCH` raw — quotes, `-`, `:`, `*`, `()` and the
//! `AND/OR/NOT` keywords would error or silently change semantics. We quote each
//! whitespace-separated term (doubling embedded quotes) so every token is a
//! literal, then join the terms with AND (titles) or OR (message bodies).

use crate::error::Result;
use crate::store::{SearchHit, Store};

/// Compile free-text input into a safe FTS5 MATCH string where every term must
/// appear (AND). Returns `None` if the input has no searchable terms.
pub fn compile_query(input: &str) -> Option<String> {
    join_terms(input, " ")
}

/// Like [`compile_query`], but any term may appear (OR); bm25 still ranks
/// messages that hold more, and rarer, terms first. A natural-language query
/// rarely has all its words in one message, so AND returns nothing for it.
pub fn compile_any_query(input: &str) -> Option<String> {
    join_terms(input, " OR ")
}

fn join_terms(input: &str, sep: &str) -> Option<String> {
    let terms: Vec<String> = input
        .split_whitespace()
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect();
    if terms.is_empty() {
        None
    } else {
        Some(terms.join(sep))
    }
}

/// Run a keyword search, returning at most `limit` results — **one per session**
/// (the best-matching message). Returns empty for an empty query.
/// `role`, when set (`"user"` / `"assistant"`), restricts matches to messages of
/// that role. Titles have no role, so the title tier is skipped when it's set.
/// `source`, when set (`"claude"` / `"codex"` / `"opencode"`), restricts matches
/// to one agent product.
pub fn search(
    store: &Store,
    query: &str,
    limit: usize,
    role: Option<&str>,
    source: Option<&str>,
) -> Result<Vec<SearchHit>> {
    let (Some(all_terms), Some(any_term)) = (compile_query(query), compile_any_query(query)) else {
        return Ok(Vec::new());
    };
    // Tiered merge (corpus-size-independent): title/alias matches first — that's
    // a strong signal and `name`'s whole purpose — then message-body matches,
    // best-first. Titles need every term (OR would float any title holding one
    // common word to the top); bodies need any term, ranked by bm25. Dedup to one result per session. (A fixed additive bm25 boost
    // is fragile because body/title bm25 scales diverge as the corpus grows.)
    let mut seen = std::collections::HashSet::new();
    let mut grouped = Vec::with_capacity(limit);

    if role.is_none() {
        for hit in store.search_titles_raw(&all_terms, limit, source)? {
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
    for hit in store.search_raw(&any_term, raw_limit, role, source)? {
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
    fn any_query_ors_quoted_terms() {
        assert_eq!(
            compile_any_query("foo OR bar").as_deref(),
            Some("\"foo\" OR \"OR\" OR \"bar\"")
        );
    }

    #[test]
    fn empty_query_is_none() {
        assert_eq!(compile_query("   "), None);
        assert_eq!(compile_any_query("   "), None);
    }
}
