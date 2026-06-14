//! SQLite-backed index: schema, idempotent per-file writes, and keyword search.
//!
//! One [`Store`] owns a single [`rusqlite::Connection`]. WAL mode allows other
//! processes to read concurrently; within a process all access goes through the
//! one connection (a CLI invocation does one job and exits).

pub mod schema;

use crate::error::Result;
use crate::index::AssembledSession;
use rusqlite::{params, Connection};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// One keyword-search result row (message-level; grouped to sessions in search step).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct SearchHit {
    pub session_id: String,
    pub title: String,
    pub project: String,
    pub line_no: i64,
    pub snippet: String,
    /// FTS5 bm25 score (lower is a better match).
    pub rank: f64,
}

/// File stat used for incremental indexing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStat {
    pub mtime_ns: i64,
    pub size: i64,
}

/// A resolved session reference (for show/resume/name/export).
#[derive(Debug, Clone, PartialEq)]
pub struct SessionRef {
    pub pk: i64,
    pub session_id: String,
    pub project_path: Option<String>,
    pub title: String,
}

/// A stored message row, for rendering a transcript.
#[derive(Debug, Clone, PartialEq)]
pub struct MessageRow {
    pub line_no: i64,
    pub rec_type: String,
    pub role: Option<String>,
    pub ts: Option<i64>,
    pub content_json: String,
}

pub struct Store {
    conn: Connection,
}

impl Store {
    /// Open (creating if needed) the index at `path`.
    pub fn open(path: &Path) -> Result<Store> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        Self::init(&conn)?;
        #[cfg(unix)]
        restrict_perms(path);
        Ok(Store { conn })
    }

    /// In-memory store for tests.
    pub fn open_in_memory() -> Result<Store> {
        let conn = Connection::open_in_memory()?;
        Self::init(&conn)?;
        Ok(Store { conn })
    }

    fn init(conn: &Connection) -> Result<()> {
        // journal_mode returns a row → read it with query_row (also confirms WAL
        // on file DBs; an in-memory DB returns "memory", which is fine).
        let _mode: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", true)?; // required for ON DELETE CASCADE
        conn.pragma_update(None, "temp_store", "MEMORY")?;
        conn.busy_timeout(std::time::Duration::from_millis(5000))?;
        schema::migrate(conn)?;
        Ok(())
    }

    /// Return the recorded stat for a file, if it has been indexed.
    pub fn file_stat(&self, file_path: &str) -> Result<Option<FileStat>> {
        match self.conn.query_row(
            "SELECT mtime_ns, size FROM session_files WHERE path=?1",
            params![file_path],
            |r| {
                Ok(FileStat {
                    mtime_ns: r.get(0)?,
                    size: r.get(1)?,
                })
            },
        ) {
            Ok(stat) => Ok(Some(stat)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Idempotently (re)write all rows for one session file in a single
    /// transaction: delete everything keyed to the file, then re-insert.
    pub fn replace_session(&mut self, s: &AssembledSession) -> Result<()> {
        let now = now_ms();
        let tx = self.conn.transaction()?;

        // CASCADE clears messages/boundaries/boundary_messages/workflows/worktrees;
        // FTS triggers fire on the cascaded message deletes. relations has no FK.
        tx.execute(
            "DELETE FROM sessions WHERE file_path=?1",
            params![s.file_path],
        )?;
        tx.execute(
            "DELETE FROM relations WHERE source_path=?1",
            params![s.file_path],
        )?;

        tx.execute(
            "INSERT INTO sessions(session_id, source_kind, file_path, project_path, project_name,
                git_branch, first_ts, last_ts, ai_title, custom_title, title,
                message_count, has_compaction, indexed_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
            params![
                s.session_id,
                s.source_kind,
                s.file_path,
                s.project_path,
                s.project_name,
                s.git_branch,
                s.first_ts,
                s.last_ts,
                s.ai_title,
                s.custom_title,
                s.title,
                s.messages.len() as i64,
                s.has_compaction as i64,
                now
            ],
        )?;
        let session_fk = tx.last_insert_rowid();

        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO messages(session_fk, uuid, parent_uuid, line_no, source_file,
                    type, subtype, role, ts, cwd, content_json, text_for_fts,
                    is_sidechain, is_compact_summary)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
            )?;
            for m in &s.messages {
                let content_json = serde_json::to_string(&m.blocks).unwrap_or_default();
                stmt.execute(params![
                    session_fk,
                    m.uuid,
                    m.parent_uuid,
                    m.line_no as i64,
                    s.file_path,
                    m.rec_type,
                    m.subtype,
                    m.role,
                    m.ts,
                    m.cwd,
                    content_json,
                    m.fts_text,
                    m.is_sidechain as i64,
                    m.is_compact_summary as i64
                ])?;
            }
        }
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO boundaries(session_fk, uuid, parent_uuid, logical_parent_uuid,
                    logical_parent_file, trigger, pre_tokens, post_tokens, ts)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            )?;
            for b in &s.boundaries {
                stmt.execute(params![
                    session_fk,
                    b.uuid,
                    b.parent_uuid,
                    b.logical_parent_uuid,
                    Option::<String>::None,
                    b.trigger,
                    b.pre_tokens,
                    b.post_tokens,
                    b.ts
                ])?;
            }
        }
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO worktrees(session_fk, original_cwd, worktree_path, worktree_name,
                    branch, original_branch, original_head, continues_session_id, link_confidence)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            )?;
            for w in &s.worktrees {
                stmt.execute(params![
                    session_fk,
                    w.original_cwd,
                    w.worktree_path,
                    w.worktree_name,
                    w.branch,
                    w.original_branch,
                    w.original_head,
                    w.continues_session_id,
                    Option::<String>::None
                ])?;
            }
        }
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO relations(parent_fk, child_fk, relation_type, evidence, confidence,
                    source_path, tool_use_id, workflow_id)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            )?;
            for br in &s.bridges {
                stmt.execute(params![
                    session_fk,
                    Option::<i64>::None,
                    "bridge",
                    br.bridge_session_id,
                    "explicit",
                    s.file_path,
                    Option::<String>::None,
                    Option::<String>::None
                ])?;
            }
        }

        tx.execute(
            "INSERT INTO session_files(path, source_kind, mtime_ns, size, parser_version,
                scan_started_at, scan_finished_at)
             VALUES (?1,?2,?3,?4,?5,?6,?6)
             ON CONFLICT(path) DO UPDATE SET
                mtime_ns=excluded.mtime_ns, size=excluded.size,
                parser_version=excluded.parser_version, scan_finished_at=excluded.scan_finished_at",
            params![
                s.file_path,
                s.source_kind,
                s.file_mtime_ns,
                s.file_size,
                PARSER_VERSION,
                now
            ],
        )?;

        tx.commit()?;
        Ok(())
    }

    /// Number of indexed sessions.
    pub fn session_count(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))?)
    }

    /// Number of indexed messages.
    pub fn message_count(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))?)
    }

    /// Number of foreign-key violations (should always be 0).
    pub fn foreign_key_violations(&self) -> Result<usize> {
        let mut stmt = self.conn.prepare("PRAGMA foreign_key_check")?;
        let count = stmt.query_map([], |_| Ok(()))?.count();
        Ok(count)
    }

    /// Run the FTS5 `integrity-check`; errors if the external-content index has
    /// drifted from the `messages` table.
    pub fn fts_integrity_check(&self) -> Result<()> {
        self.conn.execute(
            "INSERT INTO messages_fts(messages_fts, rank) VALUES('integrity-check', 1)",
            [],
        )?;
        Ok(())
    }

    /// Find sessions whose id starts with `id_prefix` (most recent first).
    /// Returns up to `limit` candidates so the caller can disambiguate.
    pub fn find_sessions(&self, id_prefix: &str, limit: usize) -> Result<Vec<SessionRef>> {
        // Escape LIKE metacharacters so `_`/`%` in the prefix are literals; the
        // trailing `%` is our wildcard. Explicit ESCAPE keeps it correct.
        let escaped = id_prefix
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let pattern = format!("{escaped}%");
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, project_path, COALESCE(title,'')
             FROM sessions WHERE session_id LIKE ?1 ESCAPE '\\'
             ORDER BY last_ts DESC NULLS LAST LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![pattern, limit as i64], |r| {
            Ok(SessionRef {
                pk: r.get(0)?,
                session_id: r.get(1)?,
                project_path: r.get(2)?,
                title: r.get(3)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Raw title/alias search over `sessions_fts` (best-first). One row per
    /// session already (titles are session-level). `line_no` is 0.
    pub fn search_titles_raw(&self, match_query: &str, limit: usize) -> Result<Vec<SearchHit>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.session_id, COALESCE(s.title,''), COALESCE(s.project_name,''),
                    0 AS line_no,
                    snippet(sessions_fts, 0, '[', ']', '…', 12),
                    bm25(sessions_fts) AS rank
             FROM sessions_fts
             JOIN sessions s ON s.id = sessions_fts.rowid
             WHERE sessions_fts MATCH ?1
             ORDER BY rank
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![match_query, limit as i64], |r| {
            Ok(SearchHit {
                session_id: r.get(0)?,
                title: r.get(1)?,
                project: r.get(2)?,
                line_no: r.get(3)?,
                snippet: r.get(4)?,
                rank: r.get(5)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Load a session's messages in line order (for `show`/`export`).
    pub fn session_messages(&self, session_pk: i64) -> Result<Vec<MessageRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT line_no, type, role, ts, COALESCE(content_json,'[]')
             FROM messages WHERE session_fk=?1 ORDER BY line_no",
        )?;
        let rows = stmt.query_map(params![session_pk], |r| {
            Ok(MessageRow {
                line_no: r.get(0)?,
                rec_type: r.get(1)?,
                role: r.get(2)?,
                ts: r.get(3)?,
                content_json: r.get(4)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Set a user alias (custom title). The `sessions_au` trigger keeps the FTS
    /// title index in sync.
    pub fn set_custom_title(&mut self, session_pk: i64, alias: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET custom_title=?2, title=?2 WHERE id=?1",
            params![session_pk, alias],
        )?;
        Ok(())
    }

    /// Raw keyword search: message-level hits ordered best-first (bm25 asc).
    /// `match_query` must already be a valid FTS5 query string (use
    /// [`crate::search::compile_query`]). Grouping to one-per-session happens in
    /// [`crate::search::search`] — FTS5 aux functions can't be nested in SQL
    /// aggregates, so dedup is done in Rust over this ordered stream.
    pub fn search_raw(&self, match_query: &str, limit: usize) -> Result<Vec<SearchHit>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.session_id, COALESCE(s.title,''), COALESCE(s.project_name,''),
                    m.line_no,
                    snippet(messages_fts, 0, '[', ']', '…', 12),
                    bm25(messages_fts) AS rank
             FROM messages_fts
             JOIN messages m ON m.id = messages_fts.rowid
             JOIN sessions s ON s.id = m.session_fk
             WHERE messages_fts MATCH ?1
             ORDER BY rank
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![match_query, limit as i64], |r| {
            Ok(SearchHit {
                session_id: r.get(0)?,
                title: r.get(1)?,
                project: r.get(2)?,
                line_no: r.get(3)?,
                snippet: r.get(4)?,
                rank: r.get(5)?,
            })
        })?;
        let mut hits = Vec::new();
        for row in rows {
            hits.push(row?);
        }
        Ok(hits)
    }
}

/// Bump when the parser's output shape changes (forces a re-parse on next index).
pub const PARSER_VERSION: i64 = 1;

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(unix)]
fn restrict_perms(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    // Best-effort: the index aggregates secrets, so keep it user-only.
    if let Ok(meta) = std::fs::metadata(path) {
        let mut perms = meta.permissions();
        perms.set_mode(0o600);
        let _ = std::fs::set_permissions(path, perms);
    }
}
