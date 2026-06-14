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

/// Run a keyword search. Returns an empty result for an empty query.
pub fn search(store: &Store, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
    match compile_query(query) {
        Some(match_query) => store.search_raw(&match_query, limit),
        None => Ok(Vec::new()),
    }
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
