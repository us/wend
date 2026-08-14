//! SQLite schema + migrations. The index is a single file holding metadata, the
//! conversation graph, and the FTS5 index. Vector tables (sqlite-vec) are added
//! in the semantic step.

use rusqlite::Connection;

/// Current schema version (stored in `PRAGMA user_version`).
pub const SCHEMA_VERSION: i64 = 5;

/// v2: session-level embedding vectors (superseded by chunk-level in v3).
const SCHEMA_V2: &str = r#"
CREATE TABLE session_vectors(
  session_fk INTEGER PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
  dim INTEGER NOT NULL,
  vec BLOB NOT NULL,
  model TEXT,
  built_at INTEGER);
"#;

/// v3: CHUNK-level semantic search. Each session is split into message-aligned
/// text chunks; each chunk gets one embedding. Vectors are raw little-endian f32
/// BLOBs (brute-force cosine in Rust — fast enough at this scale, no ANN needed).
/// `chunk_vectors` cascades off `chunks` so rebuilding a session's chunks drops
/// their vectors atomically. Replaces the v2 session-level table.
/// `IF NOT EXISTS` throughout: an intermediate dev build may have created these
/// tables while leaving `user_version` at 2, so the migration must tolerate
/// partially-present objects rather than erroring on a re-create.
const SCHEMA_V3: &str = r#"
DROP TABLE IF EXISTS session_vectors;
CREATE TABLE IF NOT EXISTS chunks(
  id INTEGER PRIMARY KEY,
  session_fk INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  ordinal INTEGER NOT NULL,
  text TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS idx_chunks_session ON chunks(session_fk);
CREATE TABLE IF NOT EXISTS chunk_vectors(
  chunk_fk INTEGER PRIMARY KEY REFERENCES chunks(id) ON DELETE CASCADE,
  dim INTEGER NOT NULL,
  dtype TEXT NOT NULL DEFAULT 'f32',
  vec BLOB NOT NULL,
  model TEXT,
  built_at INTEGER);
"#;

/// v4: force one rebuild of every chunk. `chunk_texts` used to measure a message
/// in bytes but hard-split it on that many *chars*, so 64% of real chunks
/// overshot the 1200-byte target (max seen: 2431 B ≈ 767 tokens) and were
/// silently truncated by the 512-token local model. Fixing the chunker doesn't
/// repair chunks already on disk — `sessions_without_chunks` only re-chunks
/// sessions with zero rows — so wipe them once and let the next `--embed`
/// rebuild.
///
/// Both tables are deleted explicitly rather than leaning on the cascade:
/// `foreign_keys` is switched on in `Store::open`, not in this function, which
/// is public and called directly (including by our own tests) on a bare
/// connection where the cascade would not fire and would strand vectors whose
/// `chunk_fk` a rebuilt chunk could reuse.
const SCHEMA_V4: &str = r#"
DELETE FROM chunk_vectors;
DELETE FROM chunks;
"#;

/// v5: the case library shares the `chunks` table rather than getting its own.
///
/// A *case* is a chunk whose `text` is a situation (what the agent had just done)
/// and whose `payload` is the user's verbatim reaction to it. Reusing `chunks`
/// means the embedding pipeline — model guard, batch writer, resume logic — works
/// on cases unmodified, because it only ever reads `chunks.text`.
///
/// Wrapped in an explicit transaction: `execute_batch` hands the whole string to
/// SQLite where each DDL statement autocommits, and `ALTER TABLE ADD COLUMN` has
/// no `IF NOT EXISTS`. Without this, an interruption after the first `ALTER`
/// would leave the file at v4 with one column added, and every retry would die
/// on "duplicate column name".
/// The version bump is *inside* the transaction. Committing the DDL first and
/// stamping the version afterwards leaves the same hole one level up: a crash
/// between the two gives a file that has the columns but still says v4, and
/// every later open re-runs the `ALTER`s and dies on "duplicate column name".
const SCHEMA_V5: &str = r#"
BEGIN;
ALTER TABLE chunks ADD COLUMN kind TEXT NOT NULL DEFAULT 'prose';
ALTER TABLE chunks ADD COLUMN payload TEXT;
ALTER TABLE chunks ADD COLUMN src_message_fk INTEGER;
PRAGMA user_version = 5;
COMMIT;
"#;

const SCHEMA_V1: &str = r#"
CREATE TABLE session_files(
  path TEXT PRIMARY KEY, source_kind TEXT, head_tail_hash TEXT,
  mtime_ns INTEGER, size INTEGER, last_byte_offset INTEGER,
  parser_version INTEGER, scan_started_at INTEGER, scan_finished_at INTEGER);

CREATE TABLE sessions(
  id INTEGER PRIMARY KEY, session_id TEXT, source_kind TEXT, file_path TEXT,
  project_path TEXT, project_name TEXT, git_branch TEXT,
  first_ts INTEGER, last_ts INTEGER, ai_title TEXT, custom_title TEXT,
  title TEXT NOT NULL DEFAULT '',
  message_count INTEGER, has_compaction INTEGER, indexed_at INTEGER,
  UNIQUE(source_kind, session_id, file_path));
CREATE INDEX idx_sessions_file_path ON sessions(file_path);

CREATE TABLE messages(
  id INTEGER PRIMARY KEY,
  session_fk INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  uuid TEXT, parent_uuid TEXT, line_no INTEGER, source_file TEXT,
  type TEXT, subtype TEXT, role TEXT, ts INTEGER, cwd TEXT,
  content_json TEXT, text_for_fts TEXT NOT NULL DEFAULT '',
  is_sidechain INTEGER, is_compact_summary INTEGER);
CREATE INDEX idx_messages_session ON messages(session_fk);
CREATE INDEX idx_messages_uuid ON messages(uuid);
CREATE INDEX idx_messages_parent_uuid ON messages(parent_uuid);

CREATE TABLE boundaries(
  id INTEGER PRIMARY KEY,
  session_fk INTEGER REFERENCES sessions(id) ON DELETE CASCADE,
  line_no INTEGER,
  uuid TEXT, parent_uuid TEXT, logical_parent_uuid TEXT, logical_parent_file TEXT,
  trigger TEXT, pre_tokens INTEGER, post_tokens INTEGER, ts INTEGER);
CREATE INDEX idx_boundaries_session ON boundaries(session_fk);

CREATE TABLE boundary_messages(
  boundary_fk INTEGER REFERENCES boundaries(id) ON DELETE CASCADE,
  message_fk INTEGER REFERENCES messages(id) ON DELETE CASCADE,
  path_order INTEGER, distance INTEGER, source TEXT);
CREATE INDEX idx_boundary_messages_bfk ON boundary_messages(boundary_fk);
CREATE INDEX idx_boundary_messages_mfk ON boundary_messages(message_fk);

CREATE TABLE relations(
  parent_fk INTEGER, child_fk INTEGER, relation_type TEXT,
  evidence TEXT, confidence TEXT, source_path TEXT, tool_use_id TEXT, workflow_id TEXT);
CREATE INDEX idx_relations_source_path ON relations(source_path);

CREATE TABLE workflows(
  id INTEGER PRIMARY KEY,
  parent_session_fk INTEGER REFERENCES sessions(id) ON DELETE CASCADE,
  wf_id TEXT, meta_path TEXT);
CREATE INDEX idx_workflows_session ON workflows(parent_session_fk);
CREATE TABLE workflow_events(
  workflow_fk INTEGER REFERENCES workflows(id) ON DELETE CASCADE,
  kind TEXT, ts INTEGER, payload TEXT);
CREATE INDEX idx_workflow_events_wfk ON workflow_events(workflow_fk);

CREATE TABLE worktrees(
  session_fk INTEGER REFERENCES sessions(id) ON DELETE CASCADE,
  original_cwd TEXT, worktree_path TEXT, worktree_name TEXT,
  branch TEXT, original_branch TEXT, original_head TEXT,
  continues_session_id TEXT, link_confidence TEXT);
CREATE INDEX idx_worktrees_session ON worktrees(session_fk);

-- Contentful FTS5 (stores its own copy of the text). We deliberately do NOT use
-- external-content here: with idempotent per-file delete+reinsert and reused
-- rowids, external-content + the 'delete' command drifts and `integrity-check`
-- reports SQLITE_CORRUPT_VTAB. Contentful tables support plain DELETE and stay
-- consistent across unlimited reindex cycles (verified). Cost: ~the FTS body is
-- duplicated on disk — an acceptable trade for correctness.
CREATE VIRTUAL TABLE messages_fts USING fts5(
  text_for_fts, tokenize='porter unicode61 remove_diacritics 1');
CREATE TRIGGER messages_ai AFTER INSERT ON messages WHEN new.text_for_fts <> '' BEGIN
  INSERT INTO messages_fts(rowid, text_for_fts) VALUES (new.id, new.text_for_fts);
END;
CREATE TRIGGER messages_ad AFTER DELETE ON messages WHEN old.text_for_fts <> '' BEGIN
  DELETE FROM messages_fts WHERE rowid = old.id;
END;
CREATE TRIGGER messages_au AFTER UPDATE ON messages BEGIN
  DELETE FROM messages_fts WHERE rowid = old.id;
  INSERT INTO messages_fts(rowid, text_for_fts)
    SELECT new.id, new.text_for_fts WHERE new.text_for_fts <> '';
END;

CREATE VIRTUAL TABLE sessions_fts USING fts5(title, tokenize='porter unicode61 remove_diacritics 1');
CREATE TRIGGER sessions_ai AFTER INSERT ON sessions WHEN new.title <> '' BEGIN
  INSERT INTO sessions_fts(rowid, title) VALUES (new.id, new.title);
END;
CREATE TRIGGER sessions_ad AFTER DELETE ON sessions WHEN old.title <> '' BEGIN
  DELETE FROM sessions_fts WHERE rowid = old.id;
END;
CREATE TRIGGER sessions_au AFTER UPDATE ON sessions BEGIN
  DELETE FROM sessions_fts WHERE rowid = old.id;
  INSERT INTO sessions_fts(rowid, title)
    SELECT new.id, new.title WHERE new.title <> '';
END;
"#;

/// Apply pending migrations. Idempotent: safe to call on every open.
///
/// Refuses to open an index written by a newer build. Without that check the
/// unconditional `user_version` write below would *downgrade* the stamp, and the
/// next new-enough binary would replay a destructive migration (v4 wipes every
/// chunk) — for the Azure backend that means paying to re-embed the whole
/// corpus each time an older `wend` touches the file.
pub fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version > SCHEMA_VERSION {
        return Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_MISMATCH),
            Some(format!(
                "index was written by a newer wend (schema v{version}, this build knows v{SCHEMA_VERSION}); upgrade wend"
            )),
        ));
    }
    if version < 1 {
        conn.execute_batch(SCHEMA_V1)?;
    }
    if version < 2 {
        conn.execute_batch(SCHEMA_V2)?;
    }
    if version < 3 {
        conn.execute_batch(SCHEMA_V3)?;
    }
    if version < 4 {
        conn.execute_batch(SCHEMA_V4)?;
    }
    if version < 5 {
        conn.execute_batch(SCHEMA_V5)?;
    }
    if version < SCHEMA_VERSION {
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrate_is_idempotent_on_fresh_db() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        migrate(&conn).unwrap(); // second call must not error
        let v: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
    }

    /// v4 must actually wipe pre-existing chunks *and* their vectors, so the
    /// fixed chunker rebuilds them. Both deletes are explicit rather than relying
    /// on the cascade, because `migrate` runs on connections where
    /// `foreign_keys` was never switched on — as here.
    #[test]
    fn v4_wipes_chunks_built_by_the_old_chunker() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA_V1).unwrap();
        conn.execute_batch(SCHEMA_V2).unwrap();
        conn.execute_batch(SCHEMA_V3).unwrap();
        conn.pragma_update(None, "user_version", 3_i64).unwrap();

        conn.execute_batch(
            "INSERT INTO sessions(id, session_id, title) VALUES (1,'s1','t');
             INSERT INTO chunks(id, session_fk, ordinal, text) VALUES (10,1,0,'stale oversized chunk');
             INSERT INTO chunk_vectors(chunk_fk, dim, dtype, vec, model, built_at)
               VALUES (10, 384, 'f32', x'00000000', 'multilingual-e5-small', 1);",
        )
        .unwrap();

        migrate(&conn).unwrap();

        let chunks: i64 = conn
            .query_row("SELECT count(*) FROM chunks", [], |r| r.get(0))
            .unwrap();
        let vecs: i64 = conn
            .query_row("SELECT count(*) FROM chunk_vectors", [], |r| r.get(0))
            .unwrap();
        assert_eq!(chunks, 0, "stale chunks must be wiped");
        assert_eq!(vecs, 0, "orphaned vectors must be wiped too");
        // The session itself must survive — only chunks are rebuilt.
        let sessions: i64 = conn
            .query_row("SELECT count(*) FROM sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sessions, 1);
    }

    /// v5 must add its columns to a populated v4 database without disturbing the
    /// rows already there, and existing chunks must come out as `kind='prose'`
    /// so they stay visible to ordinary semantic search.
    #[test]
    fn v5_adds_case_columns_without_touching_existing_chunks() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA_V1).unwrap();
        conn.execute_batch(SCHEMA_V2).unwrap();
        conn.execute_batch(SCHEMA_V3).unwrap();
        conn.pragma_update(None, "user_version", 4_i64).unwrap();
        conn.execute_batch(
            "INSERT INTO sessions(id, session_id, title) VALUES (1,'s1','t');
             INSERT INTO chunks(id, session_fk, ordinal, text) VALUES (7,1,0,'existing prose');",
        )
        .unwrap();

        migrate(&conn).unwrap();

        let (kind, payload, text): (String, Option<String>, String) = conn
            .query_row(
                "SELECT kind, payload, text FROM chunks WHERE id=7",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(kind, "prose", "pre-v5 chunks must default to prose");
        assert_eq!(payload, None);
        assert_eq!(text, "existing prose", "existing rows must survive intact");
    }

    /// An index written by a newer build must be refused, not silently stamped
    /// back down — a downgrade would make the next new binary replay v4 and wipe
    /// (and, on the Azure backend, re-bill) every chunk.
    #[test]
    fn migrate_refuses_a_newer_schema() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();

        assert!(migrate(&conn).is_err());
        let v: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION + 1, "must not downgrade the stamp");
    }

    /// Regression: an intermediate dev build created the chunk tables but left
    /// `user_version` at 2. The v3 migration must tolerate the pre-existing
    /// tables instead of failing with "table chunks already exists".
    #[test]
    fn migrate_tolerates_v3_tables_present_at_version_2() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA_V1).unwrap();
        conn.execute_batch(SCHEMA_V2).unwrap();
        // Simulate the drift: chunk tables already exist...
        conn.execute_batch(
            "CREATE TABLE chunks(id INTEGER PRIMARY KEY, session_fk INTEGER, ordinal INTEGER, text TEXT);
             CREATE INDEX idx_chunks_session ON chunks(session_fk);
             CREATE TABLE chunk_vectors(chunk_fk INTEGER PRIMARY KEY, dim INTEGER, dtype TEXT, vec BLOB, model TEXT, built_at INTEGER);",
        )
        .unwrap();
        // ...but the version was never bumped past 2.
        conn.pragma_update(None, "user_version", 2_i64).unwrap();

        migrate(&conn).unwrap(); // must not error
        let v: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
    }
}
