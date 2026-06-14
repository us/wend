//! Golden integration test over the committed fixture corpus. Hermetic: reads
//! only from `fixtures/`, never `~/.claude`.

use recall_core::model::{Routed, TitleUpdate};
use recall_core::parser::parse_file;
use std::path::PathBuf;

fn fixture(name: &str) -> PathBuf {
    // CARGO_MANIFEST_DIR = crates/recall-core; repo root is two levels up.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .join(name)
}

#[test]
fn basic_session_parses_into_expected_record_mix() {
    let parsed = parse_file(&fixture("basic_session.jsonl")).expect("parse");
    assert_eq!(parsed.skipped_count(), 0, "fixture must be clean");

    let mut messages = 0;
    let mut boundaries = 0;
    let mut worktrees = 0;
    let mut titles = 0;
    let mut ai_title = None;
    let mut compact_summaries = 0;

    for (_line, rec) in &parsed.records {
        match rec {
            Routed::Message(m) => {
                messages += 1;
                if m.is_compact_summary {
                    compact_summaries += 1;
                }
            }
            Routed::Boundary(_) => boundaries += 1,
            Routed::Worktree(_) => worktrees += 1,
            Routed::Title(TitleUpdate::Ai(t)) => {
                titles += 1;
                ai_title = Some(t.clone());
            }
            _ => {}
        }
    }

    // user, assistant, user(tool_result), attachment, system/turn_duration,
    // user(compact summary), assistant  = 7 message nodes
    assert_eq!(
        messages, 7,
        "graph nodes (incl. system/turn_duration + attachment)"
    );
    assert_eq!(boundaries, 1);
    assert_eq!(worktrees, 1);
    assert_eq!(titles, 1);
    assert_eq!(compact_summaries, 1, "top-level isCompactSummary detected");
    assert_eq!(
        ai_title.as_deref(),
        Some("Fix gradient explosion in training")
    );
}

#[test]
fn thinking_text_is_not_searchable_but_real_text_is() {
    let parsed = parse_file(&fixture("basic_session.jsonl")).expect("parse");
    let all_fts: String = parsed
        .records
        .iter()
        .filter_map(|(_, r)| match r {
            Routed::Message(m) => Some(m.fts_text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");

    assert!(all_fts.contains("Clip the gradients"));
    assert!(all_fts.contains("clip_grad_norm")); // from tool_result
    assert!(all_fts.contains("grep -rn")); // from tool_use input scalars
    assert!(all_fts.contains("gradient clipping added")); // from attachment file
    assert!(
        !all_fts.contains("private reasoning"),
        "thinking blocks must be excluded from FTS"
    );
}

#[test]
fn system_turn_duration_kept_as_graph_node() {
    let parsed = parse_file(&fixture("basic_session.jsonl")).expect("parse");
    let has_system_node = parsed
        .records
        .iter()
        .any(|(_, r)| matches!(r, Routed::Message(m) if m.rec_type == "system"));
    assert!(
        has_system_node,
        "system/turn_duration must be a traversable node"
    );
}
