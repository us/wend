//! SQLite schema + migrations. The index is a single file holding metadata, the
//! conversation graph, and the FTS5 index. Vector tables (sqlite-vec) are added
//! in the semantic step.

use rusqlite::Connection;

/// Current schema version (stored in `PRAGMA user_version`).
pub const SCHEMA_VERSION: i64 = 1;

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
pub fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version < 1 {
        conn.execute_batch(SCHEMA_V1)?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    }
    Ok(())
}
