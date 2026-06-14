//! Integration test: index the fixture corpus into a temp DB and assert counts,
//! idempotency (no duplicates on re-index), and that keyword search works.
//! Hermetic — uses a tempdir, never `~/.claude`.

use recall_core::index::index_all;
use recall_core::search::search;
use recall_core::store::Store;
use std::path::PathBuf;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .join(name)
}

/// Build a temp `projects/<encoded>/<session>.jsonl` tree from a fixture.
fn temp_projects() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let projects = dir.path().join("projects");
    let proj = projects.join("-Users-dev-proj");
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::copy(
        fixture("basic_session.jsonl"),
        proj.join("basic_session.jsonl"),
    )
    .unwrap();
    (dir, projects)
}

#[test]
fn indexes_fixture_and_search_finds_it() {
    let (_guard, projects) = temp_projects();
    let mut store = Store::open_in_memory().unwrap();

    let stats = index_all(&mut store, &projects, false).unwrap();
    assert_eq!(stats.files_seen, 1);
    assert_eq!(stats.indexed, 1);
    assert_eq!(store.session_count().unwrap(), 1);
    assert_eq!(
        store.message_count().unwrap(),
        7,
        "7 graph nodes from the fixture"
    );

    // keyword search hits the indexed content
    let hits = search(&store, "gradient", 10).unwrap();
    assert!(!hits.is_empty(), "expected a match for 'gradient'");
    assert!(hits.iter().any(|h| h.session_id == "basic_session"));

    // thinking text must not be searchable
    let secret = search(&store, "private reasoning", 10).unwrap();
    assert!(secret.is_empty(), "thinking blocks must not be indexed");
}

#[test]
fn reindex_is_idempotent_no_duplicates() {
    let (_guard, projects) = temp_projects();
    let mut store = Store::open_in_memory().unwrap();

    index_all(&mut store, &projects, false).unwrap();
    let sessions_after_first = store.session_count().unwrap();
    let messages_after_first = store.message_count().unwrap();

    // Full re-index 2 more times → counts must not grow (per-file replacement).
    index_all(&mut store, &projects, false).unwrap();
    index_all(&mut store, &projects, false).unwrap();
    assert_eq!(store.session_count().unwrap(), sessions_after_first);
    assert_eq!(store.message_count().unwrap(), messages_after_first);

    // Search still returns exactly one session (no duplicate rows).
    let hits = search(&store, "gradient", 50).unwrap();
    let distinct_sessions: std::collections::HashSet<_> =
        hits.iter().map(|h| h.session_id.clone()).collect();
    assert_eq!(distinct_sessions.len(), 1);

    // No orphans, FTS consistent.
    assert_eq!(store.foreign_key_violations().unwrap(), 0);
    store.fts_integrity_check().unwrap();
}

#[test]
fn recover_surfaces_pre_compaction_history() {
    use recall_core::recover::{recover_session, Item};

    let (_guard, projects) = temp_projects();
    let mut store = Store::open_in_memory().unwrap();
    index_all(&mut store, &projects, false).unwrap();
    let sess = store
        .find_sessions("basic_session", 5)
        .unwrap()
        .into_iter()
        .next()
        .unwrap();

    let rec = recover_session(&store, sess.pk).unwrap();
    assert_eq!(rec.boundary_count, 1, "fixture has one compaction boundary");
    assert_eq!(
        rec.recovered_count, 5,
        "5 message rows precede the boundary"
    );

    // The pre-compaction assistant turn must be recovered AND flagged as hidden.
    let has_flagged_pre = rec.items.iter().any(|it| {
        matches!(it, Item::Message(m)
            if m.pre_compaction && m.row.content_json.contains("Clip the gradients"))
    });
    assert!(
        has_flagged_pre,
        "pre-compaction content must be recovered + flagged"
    );

    // There is a boundary marker, and at least one post-compaction (unflagged) msg.
    assert!(rec.items.iter().any(|it| matches!(it, Item::Boundary(_))));
    assert!(rec
        .items
        .iter()
        .any(|it| matches!(it, Item::Message(m) if !m.pre_compaction)));
}

#[test]
fn name_makes_session_findable_by_alias() {
    let (_guard, projects) = temp_projects();
    let mut store = Store::open_in_memory().unwrap();
    index_all(&mut store, &projects, false).unwrap();

    let sess = store
        .find_sessions("basic_session", 5)
        .unwrap()
        .into_iter()
        .next()
        .expect("fixture session indexed");

    // A token that appears nowhere in the fixture's message content.
    let alias = "qqzz-unique-alias-token";
    assert!(
        search(&store, alias, 5).unwrap().is_empty(),
        "precondition: alias token must not exist in message bodies"
    );

    store.set_custom_title(sess.pk, alias).unwrap();

    let hits = search(&store, alias, 5).unwrap();
    assert!(
        hits.iter().any(|h| h.session_id == "basic_session"),
        "after naming, the session must be findable by its alias (title search)"
    );
    // FK + FTS stay consistent after the title update.
    assert_eq!(store.foreign_key_violations().unwrap(), 0);
    store.fts_integrity_check().unwrap();
}

#[test]
fn reindex_after_mutation_leaves_no_orphans() {
    let dir = tempfile::tempdir().unwrap();
    let projects = dir.path().join("projects");
    let proj = projects.join("-Users-dev-proj");
    std::fs::create_dir_all(&proj).unwrap();
    let file = proj.join("s.jsonl");
    std::fs::copy(fixture("basic_session.jsonl"), &file).unwrap();

    let mut store = Store::open_in_memory().unwrap();
    index_all(&mut store, &projects, false).unwrap();
    assert!(!search(&store, "gradient", 10).unwrap().is_empty());

    // Mutate: replace the file with a tiny 2-message session.
    std::fs::write(
        &file,
        "{\"type\":\"user\",\"uuid\":\"x1\",\"parentUuid\":null,\"message\":{\"role\":\"user\",\"content\":\"totally different topic\"}}\n\
         {\"type\":\"assistant\",\"uuid\":\"x2\",\"parentUuid\":\"x1\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"ok\"}]}}\n",
    )
    .unwrap();
    index_all(&mut store, &projects, false).unwrap();

    assert_eq!(store.session_count().unwrap(), 1);
    assert_eq!(store.message_count().unwrap(), 2, "old messages gone");
    // Old content no longer searchable → no orphan FTS rows.
    assert!(search(&store, "gradient", 10).unwrap().is_empty());
    assert!(!search(&store, "different topic", 10).unwrap().is_empty());
    assert_eq!(store.foreign_key_violations().unwrap(), 0);
    store.fts_integrity_check().unwrap();
}

#[test]
fn huge_limit_does_not_panic() {
    // Regression: search --limit above the internal raw cap used to panic
    // (clamp(min=limit, max=CAP) with min>max).
    let (_guard, projects) = temp_projects();
    let mut store = Store::open_in_memory().unwrap();
    index_all(&mut store, &projects, false).unwrap();
    let hits = search(&store, "gradient", 10_000_000).unwrap();
    assert!(hits.len() <= 1, "only one session in the fixture");
}

#[test]
fn alias_survives_full_reindex() {
    // Regression: a user alias (custom_title) lives only in the DB; a full
    // `index` (DELETE+reinsert) used to wipe it. It must be preserved.
    let (_guard, projects) = temp_projects();
    let mut store = Store::open_in_memory().unwrap();
    index_all(&mut store, &projects, false).unwrap();
    let sess = store
        .find_sessions("basic_session", 5)
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    store
        .set_custom_title(sess.pk, "keepme-alias-token")
        .unwrap();
    assert!(!search(&store, "keepme-alias-token", 5).unwrap().is_empty());

    // FULL reindex (not incremental) must NOT wipe the alias.
    index_all(&mut store, &projects, false).unwrap();
    let hits = search(&store, "keepme-alias-token", 5).unwrap();
    assert!(
        hits.iter().any(|h| h.session_id == "basic_session"),
        "alias must survive a full re-index"
    );
}

#[test]
fn incremental_skips_unchanged_files() {
    let (_guard, projects) = temp_projects();
    let mut store = Store::open_in_memory().unwrap();

    let first = index_all(&mut store, &projects, true).unwrap();
    assert_eq!(first.indexed, 1);
    assert_eq!(first.skipped_unchanged, 0);

    let second = index_all(&mut store, &projects, true).unwrap();
    assert_eq!(second.indexed, 0, "unchanged file must be skipped");
    assert_eq!(second.skipped_unchanged, 1);
}
