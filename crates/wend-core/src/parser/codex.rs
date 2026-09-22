//! Codex rollout-log parsing (`~/.codex/sessions/**/*.jsonl`).
//!
//! The canonical conversation lives in `event_msg` / `item_completed` items:
//! `UserMessage` (what was typed) and `AgentMessage` (what the model said).
//! `response_item` messages duplicate the same turns wrapped in large system
//! boilerplate (`<recommended_plugins>`, `<environment_context>`, skills), so
//! they are only used as a fallback when a file has no completed items at all.
//! Tool activity (`CommandExecution`, `McpToolCall`, `Extension`) is kept as
//! name-only graph nodes; `FileChange` keeps paths only (never file content);
//! `Reasoning` is encrypted server-side and carries no usable text.

use crate::model::{Block, MessageRecord, Routed};
use serde_json::Value;
use std::io::BufRead;
use std::path::Path;

/// Parsed Codex rollout file: session identity plus ordered records.
#[derive(Debug, Default)]
pub struct CodexFile {
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    pub has_compaction: bool,
    pub records: Vec<(usize, Routed)>,
    pub skipped: Vec<crate::parser::SkippedLine>,
}

impl CodexFile {
    pub fn skipped_count(&self) -> usize {
        self.skipped.len()
    }
}

/// Parse a Codex rollout file at `path`. Only failing to *open* is an error;
/// corrupt lines inside are skipped and recorded.
pub fn parse_codex_file(path: &Path) -> crate::error::Result<CodexFile> {
    let file = std::fs::File::open(path)?;
    Ok(parse_codex_reader(std::io::BufReader::new(file)))
}

/// Parse Codex rollout content from any buffered reader.
pub fn parse_codex_reader<R: BufRead>(reader: R) -> CodexFile {
    // Read all lines first: `response_item` records are only kept when the
    // file has no `item_completed` conversation at all (two-pass decision).
    let mut lines: Vec<(usize, String)> = Vec::new();
    let mut out = CodexFile::default();
    for (idx, line) in reader.lines().enumerate() {
        let line_no = idx + 1;
        match line {
            Ok(l) => {
                if !l.trim().is_empty() {
                    lines.push((line_no, l));
                }
            }
            Err(e) => {
                tracing::warn!(line_no, error = %e, "unreadable line, skipping");
                out.skipped.push(crate::parser::SkippedLine {
                    line_no,
                    reason: e.to_string(),
                });
            }
        }
    }

    let mut primary: Vec<(usize, Routed)> = Vec::new();
    let mut fallback: Vec<(usize, Routed)> = Vec::new();
    for (line_no, line) in &lines {
        let value: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(%line_no, error = %e, "corrupt json line, skipping");
                out.skipped.push(crate::parser::SkippedLine {
                    line_no: *line_no,
                    reason: e.to_string(),
                });
                continue;
            }
        };
        route_codex_line(&value, *line_no, &mut out, &mut primary, &mut fallback);
    }

    let primary_has_conversation = primary.iter().any(|(_, r)| {
        matches!(r, Routed::Message(m) if m.rec_type == "user" || m.rec_type == "assistant")
    });
    // Tool nodes are never duplicated by the fallback pass (it only produces
    // user/assistant messages), so they are always kept.
    out.records = if primary_has_conversation {
        primary
    } else {
        primary.into_iter().chain(fallback).collect()
    };
    // Every message inherits the session cwd (Codex stores it once in
    // `session_meta`, not per line like Claude Code).
    if let Some(cwd) = out.cwd.clone() {
        for (_, r) in &mut out.records {
            if let Routed::Message(m) = r {
                if m.cwd.is_none() {
                    m.cwd = Some(cwd.clone());
                }
            }
        }
    }
    out
}

fn route_codex_line(
    obj: &Value,
    line_no: usize,
    file: &mut CodexFile,
    primary: &mut Vec<(usize, Routed)>,
    fallback: &mut Vec<(usize, Routed)>,
) {
    let ty = obj.get("type").and_then(Value::as_str).unwrap_or("");
    match ty {
        "session_meta" => {
            let p = obj.get("payload");
            if file.session_id.is_none() {
                file.session_id = p
                    .and_then(|p| p.get("session_id"))
                    .and_then(Value::as_str)
                    .map(String::from);
            }
            if file.cwd.is_none() {
                file.cwd = p
                    .and_then(|p| p.get("cwd"))
                    .and_then(Value::as_str)
                    .map(String::from);
            }
        }
        "event_msg" => {
            let p = obj.get("payload");
            if p.and_then(|p| p.get("type")).and_then(Value::as_str) == Some("item_completed") {
                if let Some(item) = p.and_then(|p| p.get("item")) {
                    let ts = crate::parser::routing::ts_ms(obj);
                    route_item(item, line_no, ts, file, primary);
                }
            }
        }
        "response_item" => {
            // Fallback only (see module docs): same turns as `item_completed`
            // but wrapped in prompt boilerplate.
            if let Some(r) = route_response_message(obj, line_no) {
                fallback.push((line_no, r));
            }
        }
        "compacted" => {
            file.has_compaction = true;
        }
        _ => {}
    }
}

fn route_item(
    item: &Value,
    line_no: usize,
    ts: Option<i64>,
    file: &mut CodexFile,
    primary: &mut Vec<(usize, Routed)>,
) {
    let item_ty = item.get("type").and_then(Value::as_str).unwrap_or("");
    let id = item.get("id").and_then(Value::as_str).map(String::from);
    match item_ty {
        "UserMessage" => {
            let text = join_text_blocks(item.get("content"));
            primary.push((line_no, text_message("user", id, line_no, ts, text)));
        }
        "AgentMessage" => {
            let text = join_text_blocks(item.get("content"));
            primary.push((line_no, text_message("assistant", id, line_no, ts, text)));
        }
        "ContextCompaction" => {
            file.has_compaction = true;
        }
        "CommandExecution" => {
            // The command itself is short and genuinely searchable.
            let cmd = item
                .get("command")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default();
            let name = item
                .get("parsed_cmd")
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .and_then(|c| c.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("exec")
                .to_string();
            primary.push((
                line_no,
                tool_message(id, line_no, ts, name.clone(), join_fts(&name, &cmd), item),
            ));
        }
        "McpToolCall" => {
            let server = item.get("server").and_then(Value::as_str).unwrap_or("");
            let tool = item.get("tool").and_then(Value::as_str).unwrap_or("tool");
            let name = if server.is_empty() {
                tool.to_string()
            } else {
                format!("{server}/{tool}")
            };
            let args = item
                .get("arguments")
                .map(collect_scalars_truncated)
                .unwrap_or_default();
            primary.push((
                line_no,
                tool_message(id, line_no, ts, name.clone(), join_fts(&name, &args), item),
            ));
        }
        "Extension" => {
            let kind = item
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("extension");
            let detail = item
                .get("query")
                .and_then(Value::as_str)
                .map(String::from)
                .unwrap_or_else(|| {
                    item.get("action")
                        .map(collect_scalars_truncated)
                        .unwrap_or_default()
                });
            primary.push((
                line_no,
                tool_message(
                    id,
                    line_no,
                    ts,
                    kind.to_string(),
                    join_fts(kind, &detail),
                    item,
                ),
            ));
        }
        "FileChange" => {
            // Paths only — `changes` values can hold whole file contents.
            let paths = item
                .get("changes")
                .and_then(Value::as_object)
                .map(|m| {
                    let mut keys: Vec<&str> = m.keys().map(String::as_str).collect();
                    keys.sort();
                    keys.join("\n")
                })
                .unwrap_or_default();
            primary.push((
                line_no,
                tool_message(
                    id,
                    line_no,
                    ts,
                    "file_change".to_string(),
                    join_fts("file_change", &paths),
                    item,
                ),
            ));
        }
        _ => {
            // Reasoning (encrypted), ImageView, SubAgentActivity, turn
            // metadata, token counts: no indexable conversation text.
        }
    }
}

/// A `response_item` / `message` line with role user|assistant. Developer
/// system prompts are always skipped (prompt boilerplate, not conversation).
/// User text is stripped of injected `<recommended_plugins>` /
/// `<environment_context>` blocks; a boilerplate-only line stays a
/// traversable node with empty FTS so ordering is preserved.
fn route_response_message(obj: &Value, line_no: usize) -> Option<Routed> {
    let p = obj.get("payload")?;
    if p.get("type").and_then(Value::as_str) != Some("message") {
        return None;
    }
    let role = p.get("role").and_then(Value::as_str)?;
    let (rec_type, keep) = match role {
        "user" => ("user", true),
        "assistant" => ("assistant", true),
        _ => ("", false), // developer + system instructions: skip
    };
    if !keep {
        return None;
    }
    let raw = p
        .get("content")
        .map(|c| join_text_blocks(Some(c)))
        .unwrap_or_default();
    let text = if rec_type == "user" {
        strip_boilerplate(&raw)
    } else {
        raw.trim().to_string()
    };
    Some(text_message(
        rec_type,
        p.get("id").and_then(Value::as_str).map(String::from),
        line_no,
        crate::parser::routing::ts_ms(obj),
        text,
    ))
}

fn text_message(
    rec_type: &str,
    uuid: Option<String>,
    line_no: usize,
    ts: Option<i64>,
    text: String,
) -> Routed {
    let blocks = if text.is_empty() {
        Vec::new()
    } else {
        vec![Block::Text { text: text.clone() }]
    };
    Routed::Message(MessageRecord {
        uuid,
        parent_uuid: None,
        line_no,
        rec_type: rec_type.to_string(),
        subtype: None,
        role: Some(rec_type.to_string()),
        ts,
        cwd: None,
        is_sidechain: false,
        is_compact_summary: false,
        blocks,
        fts_text: text,
    })
}

fn tool_message(
    uuid: Option<String>,
    line_no: usize,
    ts: Option<i64>,
    name: String,
    fts_text: String,
    item: &Value,
) -> Routed {
    Routed::Message(MessageRecord {
        uuid: uuid.clone(),
        parent_uuid: None,
        line_no,
        rec_type: "tool_use".to_string(),
        subtype: None,
        role: None,
        ts,
        cwd: None,
        is_sidechain: false,
        is_compact_summary: false,
        blocks: vec![Block::ToolUse {
            id: uuid.unwrap_or_default(),
            name,
            input: item.clone(),
        }],
        fts_text,
    })
}

/// FTS body for a tool node: the tool name plus its (capped) detail. The name
/// alone would make every `exec`/`read` unfindable by what it actually did.
fn join_fts(name: &str, detail: &str) -> String {
    if detail.is_empty() {
        name.to_string()
    } else {
        format!("{name} {detail}")
    }
}

/// Join every `text` field of a content array. Codex uses `type: "text"`
/// (user) and `type: "Text"` (agent) — matched case-insensitively by just
/// reading the field.
fn join_text_blocks(content: Option<&Value>) -> String {
    let arr = match content.and_then(Value::as_array) {
        Some(a) => a,
        None => return String::new(),
    };
    arr.iter()
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Remove `<tag>…</tag>` blocks (dot-all, non-greedy). Unclosed tags run to
/// the end of the text.
fn strip_tag(text: &str, tag: &str) -> String {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(open.as_str()) {
        out.push_str(&rest[..start]);
        let after = &rest[start + open.len()..];
        rest = match after.find(close.as_str()) {
            Some(end) => &after[end + close.len()..],
            None => "",
        };
    }
    out.push_str(rest);
    out
}

fn strip_boilerplate(text: &str) -> String {
    let stripped = strip_tag(text, "recommended_plugins");
    let stripped = strip_tag(&stripped, "environment_context");
    stripped.trim().to_string()
}

/// Recursively collect string leaves, capped so tool arguments cannot flood
/// the index.
fn collect_scalars_truncated(v: &Value) -> String {
    let mut parts: Vec<&str> = Vec::new();
    collect_into(v, &mut parts);
    let joined = parts.join(" ");
    const CAP: usize = 2000;
    if joined.len() <= CAP {
        joined
    } else {
        // Cut at a char boundary.
        let mut end = CAP;
        while !joined.is_char_boundary(end) {
            end -= 1;
        }
        joined[..end].to_string()
    }
}

fn collect_into<'a>(v: &'a Value, out: &mut Vec<&'a str>) {
    match v {
        Value::String(s) => out.push(s),
        Value::Array(a) => a.iter().for_each(|x| collect_into(x, out)),
        Value::Object(m) => m.values().for_each(|x| collect_into(x, out)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn codex_line(ty: &str, payload: serde_json::Value) -> String {
        serde_json::json!({
            "timestamp": "2026-09-04T13:24:40.608Z",
            "ordinal": 1,
            "type": ty,
            "payload": payload
        })
        .to_string()
    }

    #[test]
    fn user_and_agent_items_become_messages() {
        let data = format!(
            "{}\n{}\n{}\n",
            codex_line(
                "session_meta",
                serde_json::json!({"session_id": "abc", "cwd": "/Users/x/proj"})
            ),
            codex_line(
                "event_msg",
                serde_json::json!({"type": "item_completed",
                    "item": {"type": "UserMessage", "id": "u1",
                        "content": [{"type": "text", "text": "fix the crash"}]}})
            ),
            codex_line(
                "event_msg",
                serde_json::json!({"type": "item_completed",
                    "item": {"type": "AgentMessage", "id": "a1",
                        "content": [{"type": "Text", "text": "fixed it"}]}})
            ),
        );
        let f = parse_codex_reader(Cursor::new(data));
        assert_eq!(f.session_id.as_deref(), Some("abc"));
        assert_eq!(f.cwd.as_deref(), Some("/Users/x/proj"));
        assert_eq!(f.records.len(), 2);
        match &f.records[0].1 {
            Routed::Message(m) => {
                assert_eq!(m.role.as_deref(), Some("user"));
                assert_eq!(m.fts_text, "fix the crash");
                assert_eq!(m.cwd.as_deref(), Some("/Users/x/proj"));
                assert!(m.ts.is_some());
            }
            other => panic!("expected message, got {other:?}"),
        }
        match &f.records[1].1 {
            Routed::Message(m) => assert_eq!(m.role.as_deref(), Some("assistant")),
            other => panic!("expected message, got {other:?}"),
        }
    }

    #[test]
    fn tool_items_keep_names_only_and_filechange_keeps_paths() {
        let data = format!(
            "{}\n{}\n",
            codex_line(
                "event_msg",
                serde_json::json!({"type": "item_completed",
                    "item": {"type": "McpToolCall", "id": "m1", "server": "crw",
                        "tool": "crw_search", "arguments": {"query": "x"}}})
            ),
            codex_line(
                "event_msg",
                serde_json::json!({"type": "item_completed",
                    "item": {"type": "FileChange", "id": "f1",
                        "changes": {"/a/b.rs": {"type": "add", "content": "HUGE"}}}})
            ),
        );
        let f = parse_codex_reader(Cursor::new(data));
        // No user/assistant turns → fallback path is empty too, but tool
        // nodes from the primary pass are still kept.
        assert_eq!(f.records.len(), 2);
        match &f.records[0].1 {
            Routed::Message(m) => assert!(m.fts_text.contains("crw_search")),
            other => panic!("expected tool message, got {other:?}"),
        }
        match &f.records[1].1 {
            Routed::Message(m) => {
                assert!(m.fts_text.contains("/a/b.rs"));
                assert!(!m.fts_text.contains("HUGE"));
            }
            other => panic!("expected tool message, got {other:?}"),
        }
    }

    #[test]
    fn response_items_are_ignored_when_completed_items_exist() {
        let data = format!(
            "{}\n{}\n",
            codex_line(
                "event_msg",
                serde_json::json!({"type": "item_completed",
                    "item": {"type": "UserMessage", "id": "u1",
                        "content": [{"type": "text", "text": "real text"}]}})
            ),
            codex_line(
                "response_item",
                serde_json::json!({"type": "message", "id": "m1", "role": "user",
                    "content": [{"type": "input_text", "text": "DUPLICATE"}]})
            ),
        );
        let f = parse_codex_reader(Cursor::new(data));
        assert_eq!(f.records.len(), 1, "no double-indexing");
    }

    #[test]
    fn fallback_strips_boilerplate_and_skips_developer() {
        let data = format!(
            "{}\n{}\n",
            codex_line(
                "response_item",
                serde_json::json!({"type": "message", "id": "d1", "role": "developer",
                    "content": [{"type": "input_text", "text": "You are Codex..."}]})
            ),
            codex_line(
                "response_item",
                serde_json::json!({"type": "message", "id": "m1", "role": "user",
                    "content": [{"type": "input_text",
                        "text": "<recommended_plugins>junk</recommended_plugins> <environment_context>junk</environment_context>hello"}]})
            ),
        );
        let f = parse_codex_reader(Cursor::new(data));
        assert_eq!(f.records.len(), 1, "developer skipped");
        match &f.records[0].1 {
            Routed::Message(m) => {
                assert_eq!(m.fts_text, "hello");
                assert!(!m.fts_text.contains("junk"));
            }
            other => panic!("expected message, got {other:?}"),
        }
    }

    #[test]
    fn compaction_flag_is_detected() {
        let data = format!(
            "{}\n",
            codex_line(
                "event_msg",
                serde_json::json!({"type": "item_completed",
                    "item": {"type": "ContextCompaction", "id": "c1"}})
            ),
        );
        let f = parse_codex_reader(Cursor::new(data));
        assert!(f.has_compaction);
        assert!(f.records.is_empty());
    }
}
