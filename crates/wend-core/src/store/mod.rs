//! SQLite-backed index: schema, idempotent per-file writes, and keyword search.
//!
//! One [`Store`] owns a single [`rusqlite::Connection`]. WAL mode allows other
//! processes to read concurrently; within a process all access goes through the
//! one connection (a CLI invocation does one job and exits).

pub mod schema;

use crate::error::Result;
use crate::index::AssembledSession;
use rusqlite::{params, Connection, OptionalExtension};
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

/// A lightweight session summary, for the topology view.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionBrief {
    pub pk: i64,
    pub session_id: String,
    pub project_path: Option<String>,
    pub project_name: Option<String>,
    pub title: String,
    pub last_ts: Option<i64>,
    pub message_count: i64,
}

/// A chunk vector with metadata, for brute-force semantic search.
#[derive(Debug, Clone, PartialEq)]
pub struct ChunkVec {
    pub session_id: String,
    pub title: String,
    pub project: String,
    /// The chunk's own text (used as the result snippet).
    pub text: String,
    pub vec: Vec<f32>,
}

/// A case awaiting insertion: what the agent had just done, and what the user
/// said back, verbatim.
#[derive(Debug, Clone)]
pub struct NewCase {
    pub situation: String,
    pub reaction: String,
    /// `messages.id` of the reaction, for provenance and its timestamp.
    pub src_message_fk: i64,
}

/// An embedded case, ready to rank.
#[derive(Debug, Clone)]
pub struct CaseVec {
    pub id: i64,
    pub session_id: String,
    pub project: String,
    pub situation: String,
    pub reaction: String,
    pub ts: Option<i64>,
    pub vec: Vec<f32>,
}

/// One raw transcript row, with the fields the turn assembler needs.
#[derive(Debug, Clone)]
pub struct TurnRow {
    pub id: i64,
    pub line_no: i64,
    pub role: String,
    pub ts: Option<i64>,
    pub content_json: String,
    pub is_sidechain: bool,
    pub is_compact_summary: bool,
}

/// A worktree-state record linking a session to its origin repo.
#[derive(Debug, Clone, PartialEq)]
pub struct WorktreeInfo {
    pub session_pk: i64,
    pub original_cwd: Option<String>,
    pub worktree_name: Option<String>,
    pub branch: Option<String>,
    pub continues_session_id: Option<String>,
}

/// One typed message for the flow dump (`wend messages`, self-analysis).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ProseMessage {
    pub session_id: String,
    pub project: String,
    pub title: String,
    pub ts: Option<i64>,
    pub line_no: i64,
    pub text: String,
}

/// A stored compaction-boundary row, for recovery.
#[derive(Debug, Clone, PartialEq)]
pub struct BoundaryRow {
    pub line_no: i64,
    pub trigger: Option<String>,
    pub pre_tokens: Option<i64>,
    pub post_tokens: Option<i64>,
    pub logical_parent_uuid: Option<String>,
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
        // Preserve a user-set alias (custom_title) across a full re-index — it
        // lives only in the DB, not in the .jsonl, so a plain DELETE+reinsert
        // would otherwise wipe it (data-loss footgun).
        let existing_custom: Option<String> = tx
            .query_row(
                "SELECT custom_title FROM sessions WHERE file_path=?1",
                params![s.file_path],
                |r| r.get::<_, Option<String>>(0), // column is nullable
            )
            .optional()? // None when no prior row
            .flatten(); // collapse no-row / NULL-value into None
        let custom_title = s.custom_title.clone().or(existing_custom);
        let title = custom_title
            .clone()
            .or_else(|| s.ai_title.clone())
            .unwrap_or_default();

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
                custom_title,
                title,
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
                "INSERT INTO boundaries(session_fk, line_no, uuid, parent_uuid, logical_parent_uuid,
                    logical_parent_file, trigger, pre_tokens, post_tokens, ts)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            )?;
            for b in &s.boundaries {
                stmt.execute(params![
                    session_fk,
                    b.line_no as i64,
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
        let rows = stmt.query_map(params![match_query, limit as i64], hit_from_row)?;
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

    /// All message uuids for a session (used by recovery to detect cross-file
    /// boundaries — a `logical_parent_uuid` not present here lives in another file).
    pub fn session_message_uuids(
        &self,
        session_pk: i64,
    ) -> Result<std::collections::HashSet<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT uuid FROM messages WHERE session_fk=?1 AND uuid IS NOT NULL")?;
        let rows = stmt.query_map(params![session_pk], |r| r.get::<_, String>(0))?;
        let mut set = std::collections::HashSet::new();
        for r in rows {
            set.insert(r?);
        }
        Ok(set)
    }

    /// Session pks that have no chunks yet (need chunking before embedding).
    /// Session pks with no chunks **of this kind** yet.
    ///
    /// Kind-aware on purpose: it used to test for any chunk at all, so once a
    /// session had cases it would never be prose-chunked again (or vice versa),
    /// silently shrinking whichever index ran second.
    pub fn sessions_without_chunks(&self, kind: &str) -> Result<Vec<i64>> {
        // Chronological: case building dedups reactions globally and keeps the
        // first one it sees, so "first seen" has to mean "said earliest".
        // Unordered, a later restatement processed first would suppress the
        // original — and once suppressed it never reappears.
        let mut stmt = self.conn.prepare(
            "SELECT id FROM sessions
             WHERE id NOT IN (SELECT session_fk FROM chunks WHERE kind = ?1)
             ORDER BY first_ts ASC NULLS LAST, id ASC",
        )?;
        let rows = stmt.query_map(params![kind], |r| r.get(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Ordered text of the user's OWN prompts for a session (assistant/tool/log/
    /// boilerplate excluded) — the input to chunking. What you asked defines the
    /// topic and keeps the embed corpus small and fast.
    pub fn semantic_messages(&self, session_pk: i64) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT text_for_fts FROM messages
             WHERE session_fk=?1 AND text_for_fts<>''
               AND type='user' AND content_json LIKE '[{\"kind\":\"text\"%'
               AND text_for_fts NOT LIKE '<%' AND text_for_fts NOT LIKE '/%'
             ORDER BY line_no",
        )?;
        let rows = stmt.query_map(params![session_pk], |r| r.get(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Dump typed prose messages for one `role` across ALL sessions, in flow
    /// order (session by first activity, then line order). Same "real prompt"
    /// filter as [`Self::semantic_messages`], plus: no subagent turns
    /// (`is_sidechain`) and no bracketed system/attachment markers (`[request
    /// interrupted…]`, `[image: …]`). First block is text, no tool results /
    /// system reminders (`<…>`) / slash commands (`/…`). `limit` caps the total;
    /// `None` dumps everything.
    pub fn list_prose_messages(
        &self,
        role: &str,
        limit: Option<usize>,
    ) -> Result<Vec<ProseMessage>> {
        let limit_clause = if limit.is_some() { " LIMIT ?2" } else { "" };
        let mut stmt = self.conn.prepare(&format!(
            "SELECT s.session_id, COALESCE(s.project_name,''), COALESCE(s.title,''),
                    m.ts, m.line_no, m.text_for_fts
             FROM messages m JOIN sessions s ON s.id = m.session_fk
             WHERE m.role = ?1 AND m.text_for_fts <> '' AND m.is_sidechain = 0
               AND m.content_json LIKE '[{{\"kind\":\"text\"%'
               AND m.text_for_fts NOT LIKE '<%' AND m.text_for_fts NOT LIKE '/%'
               AND m.text_for_fts NOT LIKE '[%'
             ORDER BY s.first_ts, s.id, m.line_no{limit_clause}"
        ))?;
        let map = |r: &rusqlite::Row| {
            Ok(ProseMessage {
                session_id: r.get(0)?,
                project: r.get(1)?,
                title: r.get(2)?,
                ts: r.get(3)?,
                line_no: r.get(4)?,
                text: r.get(5)?,
            })
        };
        let rows = match limit {
            Some(n) => stmt.query_map(params![role, n as i64], map)?,
            None => stmt.query_map(params![role], map)?,
        };
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Insert a chunk, returning its id.
    /// Insert one session's complete chunk set in a single transaction.
    ///
    /// Atomic on purpose: [`Self::sessions_without_chunks`] only resurfaces
    /// sessions with *zero* chunk rows, so a crash partway through a row-at-a-time
    /// insert would leave that session permanently half-chunked — it never looks
    /// unfinished again, and the rest of its messages stay unsearchable forever.
    pub fn insert_session_chunks(&mut self, session_pk: i64, texts: &[String]) -> Result<usize> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO chunks(session_fk, ordinal, text, kind) VALUES (?1,?2,?3,'prose')",
            )?;
            for (ordinal, text) in texts.iter().enumerate() {
                stmt.execute(params![session_pk, ordinal as i64, text])?;
            }
        }
        tx.commit()?;
        Ok(texts.len())
    }

    /// Insert one session's complete case set in a single transaction.
    ///
    /// Atomic for the same reason prose chunking is: [`Self::sessions_without_chunks`]
    /// only resurfaces sessions with zero rows of the kind, so a half-written
    /// session would never look unfinished again.
    pub fn insert_session_cases(&mut self, session_pk: i64, cases: &[NewCase]) -> Result<usize> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO chunks(session_fk, ordinal, text, kind, payload, src_message_fk)
                 VALUES (?1,?2,?3,'case',?4,?5)",
            )?;
            for (ordinal, c) in cases.iter().enumerate() {
                stmt.execute(params![
                    session_pk,
                    ordinal as i64,
                    c.situation,
                    c.reaction,
                    c.src_message_fk
                ])?;
            }
        }
        tx.commit()?;
        Ok(cases.len())
    }

    /// Every embedded case, with the payload and provenance `ChunkVec` lacks.
    pub fn case_vectors(&self, model: &str) -> Result<Vec<CaseVec>> {
        let mut stmt = self.conn.prepare(
            "SELECT c.id, s.session_id, COALESCE(s.project_name,''), c.text,
                    COALESCE(c.payload,''), m.ts, v.vec
             FROM chunk_vectors v
             JOIN chunks c   ON c.id = v.chunk_fk
             JOIN sessions s ON s.id = c.session_fk
             LEFT JOIN messages m ON m.id = c.src_message_fk
             WHERE v.model IS ?1 AND c.kind = 'case'",
        )?;
        let rows = stmt.query_map(params![model], |r| {
            let blob: Vec<u8> = r.get(6)?;
            Ok(CaseVec {
                id: r.get(0)?,
                session_id: r.get(1)?,
                project: r.get(2)?,
                situation: r.get(3)?,
                reaction: r.get(4)?,
                ts: r.get(5)?,
                vec: bytes_to_f32(&blob),
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Every reaction already stored as a case, for cross-session dedup.
    pub fn existing_case_reactions(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT COALESCE(payload,'') FROM chunks WHERE kind='case'")?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Raw rows for one session, in line order, for the turn assembler.
    ///
    /// Distinct from [`Self::session_messages`] because the assembler needs the
    /// row id (case provenance) and `is_sidechain` (so subagent turns are not
    /// spliced into a main-thread case), neither of which that accessor exposes.
    pub fn session_turn_rows(&self, session_pk: i64) -> Result<Vec<TurnRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, line_no, COALESCE(role,''), ts, COALESCE(content_json,'[]'),
                    COALESCE(is_sidechain,0), COALESCE(is_compact_summary,0)
             FROM messages WHERE session_fk=?1 ORDER BY line_no",
        )?;
        let rows = stmt.query_map(params![session_pk], |r| {
            Ok(TurnRow {
                id: r.get(0)?,
                line_no: r.get(1)?,
                role: r.get(2)?,
                ts: r.get(3)?,
                content_json: r.get(4)?,
                is_sidechain: r.get::<_, i64>(5)? != 0,
                is_compact_summary: r.get::<_, i64>(6)? != 0,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Chunks that still need an embedding under `model` (id + text). Resume-safe.
    ///
    /// Covers both "never embedded" and "embedded by a different model", so
    /// switching backends re-embeds instead of silently mixing incompatible
    /// vector spaces. `IS NOT` rather than `<>` so a NULL `model` also counts as
    /// stale (SQLite's `<>` yields NULL there, which would never match).
    ///
    /// `kind` filters to one chunk kind; `None` means every kind, which is what
    /// the backfill wants — miss it and one kind is never embedded and never
    /// costed.
    pub fn chunks_needing_vectors(
        &self,
        model: &str,
        kind: Option<&str>,
    ) -> Result<Vec<(i64, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT c.id, c.text FROM chunks c
             LEFT JOIN chunk_vectors v ON v.chunk_fk = c.id
             WHERE (v.chunk_fk IS NULL OR v.model IS NOT ?1)
               AND (?2 IS NULL OR c.kind = ?2)",
        )?;
        let rows = stmt.query_map(params![model, kind], |r| Ok((r.get(0)?, r.get(1)?)))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Store a batch of chunk vectors in a single transaction.
    ///
    /// Batched because a full backfill is ~20k rows and autocommit-per-row is
    /// the dominant cost. The transaction deliberately covers only these writes,
    /// never the network call that produced them — holding a write txn open
    /// across a multi-second retry backoff buys nothing.
    pub fn store_chunk_vectors_batch(
        &mut self,
        model: &str,
        vectors: &[(i64, Vec<f32>)],
    ) -> Result<usize> {
        let ts = now_ms();
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO chunk_vectors(chunk_fk, dim, dtype, vec, model, built_at)
                 VALUES (?1,?2,'f32',?3,?4,?5)
                 ON CONFLICT(chunk_fk) DO UPDATE SET
                    dim=excluded.dim, vec=excluded.vec,
                    model=excluded.model, built_at=excluded.built_at",
            )?;
            for (chunk_id, vec) in vectors {
                let blob: Vec<u8> = vec.iter().flat_map(|f| f.to_le_bytes()).collect();
                stmt.execute(params![chunk_id, vec.len() as i64, blob, model, ts])?;
            }
        }
        tx.commit()?;
        Ok(vectors.len())
    }

    /// Number of chunks embedded under `model` (`None` = every model).
    pub fn chunk_vector_count(&self, model: Option<&str>) -> Result<i64> {
        Ok(match model {
            Some(m) => self.conn.query_row(
                "SELECT COUNT(*) FROM chunk_vectors WHERE model IS ?1",
                params![m],
                |r| r.get(0),
            )?,
            None => self
                .conn
                .query_row("SELECT COUNT(*) FROM chunk_vectors", [], |r| r.get(0))?,
        })
    }

    /// All chunk vectors + metadata for `model`, for brute-force semantic search.
    ///
    /// Filtering by model is a correctness requirement, not a nicety: `dot()`
    /// zips to the shorter of the two vectors, so scoring a 1024-d query against
    /// a leftover 384-d vector yields a plausible-looking but meaningless number
    /// rather than an error.
    /// `kind` keeps cases out of ordinary prose search and vice versa — without
    /// it `wend search --semantic` would start returning situation text.
    pub fn all_chunk_vectors(&self, model: &str, kind: &str) -> Result<Vec<ChunkVec>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.session_id, COALESCE(s.title,''), COALESCE(s.project_name,''), c.text, v.vec
             FROM chunk_vectors v JOIN chunks c ON c.id=v.chunk_fk JOIN sessions s ON s.id=c.session_fk
             WHERE v.model IS ?1 AND c.kind = ?2",
        )?;
        let rows = stmt.query_map(params![model, kind], |r| {
            let blob: Vec<u8> = r.get(4)?;
            Ok(ChunkVec {
                session_id: r.get(0)?,
                title: r.get(1)?,
                project: r.get(2)?,
                text: r.get(3)?,
                vec: bytes_to_f32(&blob),
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// All sessions as lightweight summaries (most recent first) for topology.
    pub fn all_sessions(&self) -> Result<Vec<SessionBrief>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, project_path, project_name, COALESCE(title,''),
                    last_ts, COALESCE(message_count,0)
             FROM sessions ORDER BY last_ts DESC NULLS LAST",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(SessionBrief {
                pk: r.get(0)?,
                session_id: r.get(1)?,
                project_path: r.get(2)?,
                project_name: r.get(3)?,
                title: r.get(4)?,
                last_ts: r.get(5)?,
                message_count: r.get(6)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// All worktree-state records (one or more per worktree session).
    pub fn all_worktrees(&self) -> Result<Vec<WorktreeInfo>> {
        let mut stmt = self.conn.prepare(
            "SELECT session_fk, original_cwd, worktree_name, branch, continues_session_id
             FROM worktrees",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(WorktreeInfo {
                session_pk: r.get(0)?,
                original_cwd: r.get(1)?,
                worktree_name: r.get(2)?,
                branch: r.get(3)?,
                continues_session_id: r.get(4)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Load a session's compaction boundaries in line order (for recovery).
    pub fn session_boundaries(&self, session_pk: i64) -> Result<Vec<BoundaryRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT line_no, trigger, pre_tokens, post_tokens, logical_parent_uuid
             FROM boundaries WHERE session_fk=?1 ORDER BY line_no",
        )?;
        let rows = stmt.query_map(params![session_pk], |r| {
            Ok(BoundaryRow {
                line_no: r.get(0)?,
                trigger: r.get(1)?,
                pre_tokens: r.get(2)?,
                post_tokens: r.get(3)?,
                logical_parent_uuid: r.get(4)?,
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
    /// `role`, when set (`"user"` / `"assistant"`), restricts hits to messages
    /// with that role — the prose you typed vs. what the model said. Tool
    /// content isn't a role (it's blocks flattened into the user/assistant
    /// message's FTS body), so it can't be isolated here.
    pub fn search_raw(
        &self,
        match_query: &str,
        limit: usize,
        role: Option<&str>,
    ) -> Result<Vec<SearchHit>> {
        let role_clause = if role.is_some() {
            " AND m.role = ?3"
        } else {
            ""
        };
        let mut stmt = self.conn.prepare(&format!(
            "SELECT s.session_id, COALESCE(s.title,''), COALESCE(s.project_name,''),
                    m.line_no,
                    snippet(messages_fts, 0, '[', ']', '…', 12),
                    bm25(messages_fts) AS rank
             FROM messages_fts
             JOIN messages m ON m.id = messages_fts.rowid
             JOIN sessions s ON s.id = m.session_fk
             WHERE messages_fts MATCH ?1{role_clause}
             ORDER BY rank
             LIMIT ?2"
        ))?;
        let rows = match role {
            Some(role) => stmt.query_map(params![match_query, limit as i64, role], hit_from_row)?,
            None => stmt.query_map(params![match_query, limit as i64], hit_from_row)?,
        };
        let mut hits = Vec::new();
        for row in rows {
            hits.push(row?);
        }
        Ok(hits)
    }
}

/// Bump when the parser's output shape changes (forces a re-parse on next index).
pub const PARSER_VERSION: i64 = 1;

/// Build a [`SearchHit`] from a search row (columns in the shared select order).
fn hit_from_row(r: &rusqlite::Row) -> rusqlite::Result<SearchHit> {
    Ok(SearchHit {
        session_id: r.get(0)?,
        title: r.get(1)?,
        project: r.get(2)?,
        line_no: r.get(3)?,
        snippet: r.get(4)?,
        rank: r.get(5)?,
    })
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Decode a little-endian f32 BLOB back into a vector.
fn bytes_to_f32(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
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

impl Store {
    /// Compaction summaries for a session, oldest first.
    ///
    /// Claude Code writes one whenever a session runs out of context; each is a
    /// structured account of the goal and what is still open. They have always
    /// been stored and never read.
    pub fn compact_summaries(&self, session_pk: i64) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT text_for_fts FROM messages
             WHERE session_fk=?1 AND is_compact_summary=1 AND text_for_fts<>''
             ORDER BY line_no",
        )?;
        let rows = stmt.query_map(params![session_pk], |r| r.get(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// The first thing the user actually typed in a session.
    ///
    /// The fallback when there is no compaction summary, and the only claim that
    /// can be made honestly without one: this is what they opened with.
    pub fn first_spoken_message(&self, session_pk: i64) -> Result<Option<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT text_for_fts FROM messages
             WHERE session_fk=?1 AND type='user' AND is_sidechain=0
               AND COALESCE(is_compact_summary,0)=0
               AND content_json LIKE '[{\"kind\":\"text\"%'
               AND text_for_fts NOT LIKE '<%' AND text_for_fts NOT LIKE '/%'
               AND length(text_for_fts) > 20
             ORDER BY line_no LIMIT 1",
        )?;
        let mut rows = stmt.query_map(params![session_pk], |r| r.get::<_, String>(0))?;
        Ok(match rows.next() {
            Some(r) => Some(r?),
            None => None,
        })
    }
}
