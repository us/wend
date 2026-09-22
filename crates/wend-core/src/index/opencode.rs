//! Opencode sessions (`~/.local/share/opencode/opencode.db`).
//!
//! Opencode keeps every session in a SQLite database: `session` rows carry the
//! id, working directory and title; `message` rows carry the role; `part` rows
//! carry the content (`text` prose, `reasoning`, `tool` calls, `patch` file
//! lists, `file` attachments). We read that database **read-only** and fold
//! each message's parts into one [`AssembledSession`] message, mirroring the
//! Claude Code shape (prose is searchable, reasoning/images are kept but not
//! indexed, tool calls keep their name only).

use crate::index::AssembledSession;
use crate::model::{Block, MessageRecord};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use std::path::Path;

/// Synthetic `file_path` prefix for opencode sessions (they live in a
/// database, not in files). Never collides with real transcript paths.
pub fn opencode_file_path(session_id: &str) -> String {
    format!("opencode:{session_id}")
}

/// Read every opencode session from `db_path` (opened read-only; the live
/// opencode process is never disturbed). Sessions are returned oldest-first.
pub fn read_opencode_sessions(db_path: &Path) -> crate::error::Result<Vec<AssembledSession>> {
    let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut sessions: Vec<RawSession> = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT id, directory, title, time_created, time_updated, time_compacting
             FROM session ORDER BY time_created",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(RawSession {
                id: r.get(0)?,
                directory: r.get(1)?,
                title: r.get(2)?,
                time_updated: r.get(4)?,
                time_compacting: r.get::<_, Option<i64>>(5)?,
            })
        })?;
        for row in rows {
            sessions.push(row?);
        }
    }

    let mut out = Vec::with_capacity(sessions.len());
    for s in sessions {
        let messages = read_messages(&conn, &s.id)?;
        out.push(assemble_opencode(s, messages));
    }
    Ok(out)
}

struct RawSession {
    id: String,
    directory: String,
    title: String,
    time_updated: i64,
    time_compacting: Option<i64>,
}

struct RawMessage {
    role: String,
    time_created: i64,
    parts: Vec<Value>,
}

fn read_messages(conn: &Connection, session_id: &str) -> crate::error::Result<Vec<RawMessage>> {
    let mut ids: Vec<(String, String, i64)> = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT id, data, time_created FROM message
             WHERE session_id=?1 ORDER BY time_created, id",
        )?;
        let rows = stmt.query_map([session_id], |r| {
            let id: String = r.get(0)?;
            let data: String = r.get(1)?;
            let ts: i64 = r.get(2)?;
            Ok((id, data, ts))
        })?;
        for row in rows {
            ids.push(row?);
        }
    }
    let mut out = Vec::with_capacity(ids.len());
    for (mid, data, ts) in ids {
        let v: Value = serde_json::from_str(&data).unwrap_or(Value::Null);
        let role = v
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("assistant")
            .to_string();
        let time_created = v
            .get("time")
            .and_then(|t| t.get("created"))
            .and_then(Value::as_i64)
            .unwrap_or(ts);
        let mut parts: Vec<(i64, String, Value)> = Vec::new();
        {
            let mut pstmt = conn.prepare(
                "SELECT data, time_created FROM part
                 WHERE message_id=?1 ORDER BY time_created, id",
            )?;
            let prows = pstmt.query_map([mid.as_str()], |r| {
                let data: String = r.get(0)?;
                let pts: i64 = r.get(1)?;
                Ok((pts, data))
            })?;
            for row in prows {
                let (pts, data) = row?;
                let pv: Value = serde_json::from_str(&data).unwrap_or(Value::Null);
                parts.push((pts, data, pv));
            }
        }
        // Sort defensively by timestamp (the ORDER BY above already does).
        parts.sort_by_key(|(pts, _, _)| *pts);
        out.push(RawMessage {
            role,
            time_created,
            parts: parts.into_iter().map(|(_, _, v)| v).collect(),
        });
    }
    Ok(out)
}

fn assemble_opencode(s: RawSession, messages: Vec<RawMessage>) -> AssembledSession {
    let project_name = Path::new(&s.directory)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned());
    let mut records: Vec<MessageRecord> = Vec::new();
    let mut first_ts: Option<i64> = None;
    let mut last_ts: Option<i64> = None;
    for (idx, m) in messages.into_iter().enumerate() {
        let ts = Some(m.time_created);
        first_ts = Some(first_ts.map_or(m.time_created, |f: i64| f.min(m.time_created)));
        last_ts = Some(last_ts.map_or(m.time_created, |l: i64| l.max(m.time_created)));
        let rec_type = match m.role.as_str() {
            "user" => "user",
            _ => "assistant",
        };
        let mut fts_parts: Vec<String> = Vec::new();
        let mut blocks: Vec<Block> = Vec::new();
        for p in &m.parts {
            let (fts, block) = map_part(p);
            if !fts.is_empty() {
                fts_parts.push(fts);
            }
            if let Some(b) = block {
                blocks.push(b);
            }
        }
        // Skip pure-protocol messages (step markers with no content) but keep
        // empty prose shells out too: no blocks and no text → no row.
        if blocks.is_empty() && fts_parts.is_empty() {
            continue;
        }
        records.push(MessageRecord {
            uuid: None,
            parent_uuid: None,
            line_no: idx + 1,
            rec_type: rec_type.to_string(),
            subtype: None,
            role: Some(rec_type.to_string()),
            ts,
            cwd: Some(s.directory.clone()),
            is_sidechain: false,
            is_compact_summary: false,
            blocks,
            fts_text: fts_parts.join("\n"),
        });
    }
    // Renumber surviving rows so `show` numbering stays dense.
    for (i, m) in records.iter_mut().enumerate() {
        m.line_no = i + 1;
    }
    let title = s.title.clone();
    AssembledSession {
        session_id: s.id.clone(),
        source_kind: "opencode".to_string(),
        file_path: opencode_file_path(&s.id),
        project_path: Some(s.directory),
        project_name,
        git_branch: None,
        first_ts,
        last_ts,
        ai_title: Some(title.clone()),
        custom_title: None,
        title,
        has_compaction: s.time_compacting.is_some(),
        messages: records,
        boundaries: Vec::new(),
        worktrees: Vec::new(),
        bridges: Vec::new(),
        file_mtime_ns: s.time_updated.saturating_mul(1_000_000),
        file_size: 0,
    }
}

/// Map one opencode part to `(fts_text, block)`. Reasoning and images are
/// kept for rendering but excluded from search; tool calls keep their name
/// only (arguments/outputs can be huge); patches keep file paths only.
fn map_part(p: &Value) -> (String, Option<Block>) {
    let ty = p.get("type").and_then(Value::as_str).unwrap_or("");
    match ty {
        "text" => {
            let text = p.get("text").and_then(Value::as_str).unwrap_or("");
            if text.is_empty() {
                (String::new(), None)
            } else {
                (
                    text.to_string(),
                    Some(Block::Text {
                        text: text.to_string(),
                    }),
                )
            }
        }
        "reasoning" => {
            let text = p
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            (String::new(), Some(Block::Thinking { text }))
        }
        "tool" => {
            let name = p.get("tool").and_then(Value::as_str).unwrap_or("tool");
            let id = p
                .get("callID")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            (
                name.to_string(),
                Some(Block::ToolUse {
                    id,
                    name: name.to_string(),
                    input: Value::Null,
                }),
            )
        }
        "patch" => {
            let files = p
                .get("files")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default();
            if files.is_empty() {
                (String::new(), None)
            } else {
                let text = format!("edited files:\n{files}");
                (text.clone(), Some(Block::Text { text }))
            }
        }
        "file" => {
            let mime = p
                .get("mime")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            // Attachments are often base64 screenshots — never index the bytes.
            let byte_len = p
                .get("url")
                .and_then(Value::as_str)
                .map(|u| u.len() * 3 / 4)
                .unwrap_or(0);
            (
                String::new(),
                Some(Block::Image {
                    media_type: mime,
                    byte_len,
                }),
            )
        }
        _ => (String::new(), None), // step-start, step-finish, unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn part(ty: &str, extra: serde_json::Value) -> Value {
        let mut m = serde_json::Map::new();
        m.insert("type".to_string(), Value::String(ty.to_string()));
        if let Value::Object(e) = extra {
            m.extend(e);
        }
        Value::Object(m)
    }

    #[test]
    fn text_is_searchable_reasoning_is_not() {
        let (fts, block) = map_part(&part("text", serde_json::json!({"text": "hello"})));
        assert_eq!(fts, "hello");
        assert!(matches!(block, Some(Block::Text { .. })));

        let (fts, block) = map_part(&part(
            "reasoning",
            serde_json::json!({"text": "secret chain"}),
        ));
        assert_eq!(fts, "");
        assert!(matches!(block, Some(Block::Thinking { .. })));
    }

    #[test]
    fn tool_keeps_name_only_patch_keeps_paths() {
        let (fts, _) = map_part(&part(
            "tool",
            serde_json::json!({"tool": "read", "callID": "c1"}),
        ));
        assert_eq!(fts, "read");

        let (fts, _) = map_part(&part("patch", serde_json::json!({"files": ["/a/b.rs"]})));
        assert!(fts.contains("/a/b.rs"));
    }

    #[test]
    fn file_part_never_indexes_bytes() {
        let (fts, block) = map_part(&part(
            "file",
            serde_json::json!({"mime": "image/png", "url": "data:image/png;base64,AAAA"}),
        ));
        assert_eq!(fts, "");
        assert!(matches!(block, Some(Block::Image { .. })));
    }

    /// End-to-end over a scratch opencode database: sessions, message order,
    /// part folding, and protocol-only messages being dropped.
    #[test]
    fn reads_a_scratch_opencode_db() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("opencode.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE session(id TEXT PRIMARY KEY, project_id TEXT, directory TEXT, title TEXT,
                version TEXT, slug TEXT, time_created INTEGER, time_updated INTEGER, time_compacting INTEGER);
             CREATE TABLE message(id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER,
                time_updated INTEGER, data TEXT);
             CREATE TABLE part(id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT,
                time_created INTEGER, time_updated INTEGER, data TEXT);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session(id, project_id, directory, title, version, slug, time_created, time_updated, time_compacting)
             VALUES ('ses_1','global','/Users/x/proj','My title','v','s',1000,2000,NULL)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message(id, session_id, time_created, time_updated, data)
             VALUES ('m1','ses_1',1000,1001,'{\"role\":\"user\",\"time\":{\"created\":1000}}')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO part(id, message_id, session_id, time_created, time_updated, data)
             VALUES ('p1','m1','ses_1',1000,1001,'{\"type\":\"text\",\"text\":\"do the thing\"}')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message(id, session_id, time_created, time_updated, data)
             VALUES ('m2','ses_1',1002,1003,'{\"role\":\"assistant\",\"time\":{\"created\":1002}}')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO part(id, message_id, session_id, time_created, time_updated, data)
             VALUES ('p2','m2','ses_1',1002,1003,'{\"type\":\"step-start\"}')",
            [],
        )
        .unwrap();
        drop(conn);

        let sessions = read_opencode_sessions(&db_path).unwrap();
        assert_eq!(sessions.len(), 1);
        let s = &sessions[0];
        assert_eq!(s.session_id, "ses_1");
        assert_eq!(s.source_kind, "opencode");
        assert_eq!(s.title, "My title");
        assert_eq!(s.project_path.as_deref(), Some("/Users/x/proj"));
        // The step-start-only assistant message is dropped; the user one stays.
        assert_eq!(s.messages.len(), 1);
        assert_eq!(s.messages[0].fts_text, "do the thing");
        assert_eq!(s.messages[0].role.as_deref(), Some("user"));
    }
}
