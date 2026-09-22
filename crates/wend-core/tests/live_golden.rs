//! Integration test: live search scans the raw fixture files with no index.
//! Hermetic — uses a tempdir, never `~/.claude`.

use std::path::PathBuf;
use wend_core::index::Sources;
use wend_core::live::search_live;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .join(name)
}

/// Temp dirs shaped like the real sources: `projects/<encoded>/<session>.jsonl`
/// plus an empty codex dir, no opencode db.
fn temp_sources() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let projects = dir.path().join("projects");
    let proj = projects.join("-Users-dev-proj");
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::copy(
        fixture("basic_session.jsonl"),
        proj.join("basic_session.jsonl"),
    )
    .unwrap();
    let codex = dir.path().join("codex");
    std::fs::create_dir_all(&codex).unwrap();
    (dir, projects, codex)
}

fn sources<'a>(projects: &'a std::path::Path, codex: &'a std::path::Path) -> Sources<'a> {
    Sources {
        claude: projects,
        codex,
        opencode_db: None,
    }
}

#[test]
fn live_search_finds_fixture_term_with_no_index() {
    let (_guard, projects, codex) = temp_sources();
    let hits = search_live(&sources(&projects, &codex), "gradient", 10, None, None).unwrap();
    assert_eq!(hits.len(), 1, "one session matches");
    assert_eq!(hits[0].session_id, "basic_session");
    assert_eq!(hits[0].source, "claude");
    assert!(!hits[0].snippet.is_empty());
}

#[test]
fn live_search_role_filter_restricts_to_one_side() {
    let (_guard, projects, codex) = temp_sources();
    // "in place" is only in the assistant reply (the tool_result line and
    // the user lines don't contain it).
    let asst = search_live(
        &sources(&projects, &codex),
        "in place",
        10,
        Some("assistant"),
        None,
    )
    .unwrap();
    assert_eq!(asst.len(), 1);
    assert_eq!(asst[0].role, "assistant");

    let user = search_live(
        &sources(&projects, &codex),
        "in place",
        10,
        Some("user"),
        None,
    )
    .unwrap();
    assert!(user.is_empty(), "no user message mentions it");
}

#[test]
fn live_search_empty_query_and_source_filter() {
    let (_guard, projects, codex) = temp_sources();
    let empty = search_live(&sources(&projects, &codex), "   ", 10, None, None).unwrap();
    assert!(empty.is_empty());

    let codex_only = search_live(
        &sources(&projects, &codex),
        "gradient",
        10,
        None,
        Some("codex"),
    )
    .unwrap();
    assert!(codex_only.is_empty(), "no codex files in the temp dir");

    let claude_only = search_live(
        &sources(&projects, &codex),
        "gradient",
        10,
        None,
        Some("claude"),
    )
    .unwrap();
    assert_eq!(claude_only.len(), 1);
}
