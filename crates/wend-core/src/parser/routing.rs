//! Route a parsed JSON line into a [`Routed`] record.
//!
//! Key rules (verified against real `~/.claude/projects` data):
//! - content for user/assistant lives at `message.content`; for `attachment` at
//!   `attachment.content`; for `progress` at `data` (usually empty FTS).
//! - `isCompactSummary` is a **top-level** field, not under `message`.
//! - all non-boundary `system/*` lines are kept as traversable graph nodes
//!   (empty FTS) — they parent real turns, so dropping them breaks recovery.

use crate::model::{
    Block, BoundaryRecord, BridgeRecord, MessageRecord, Routed, TitleUpdate, WorktreeRecord,
};
use crate::parser::content::flatten_content;
use serde_json::Value;

/// Route one line. `line_no` is 1-based.
pub(crate) fn route(obj: &Value, line_no: usize) -> Routed {
    let ty = obj.get("type").and_then(Value::as_str).unwrap_or("");
    match ty {
        "user" | "assistant" => message_node(obj, ty, line_no, message_content_flatten(obj)),
        "attachment" => message_node(obj, ty, line_no, attachment_flatten(obj)),
        "progress" => message_node(obj, ty, line_no, (String::new(), Vec::new())),
        "system" => route_system(obj, line_no),
        "worktree-state" => Routed::Worktree(worktree(obj, line_no)),
        "bridge-session" => Routed::Bridge(bridge(obj, line_no)),
        "ai-title" => match obj.get("aiTitle").and_then(Value::as_str) {
            Some(t) => Routed::Title(TitleUpdate::Ai(t.to_string())),
            None => Routed::Skip,
        },
        "custom-title" => match obj.get("customTitle").and_then(Value::as_str) {
            Some(t) => Routed::Title(TitleUpdate::Custom(t.to_string())),
            None => Routed::Skip,
        },
        // Recognized-but-not-stored metadata lines.
        "agent-name"
        | "last-prompt"
        | "permission-mode"
        | "mode"
        | "pr-link"
        | "file-history-snapshot"
        | "queue-operation"
        | "agent-setting" => Routed::Skip,
        // Unknown line types: keep them as traversable nodes so the graph stays
        // connected if a future type ever parents a real turn.
        _ => message_node(obj, ty, line_no, (String::new(), Vec::new())),
    }
}

fn route_system(obj: &Value, line_no: usize) -> Routed {
    let subtype = obj.get("subtype").and_then(Value::as_str).unwrap_or("");
    if subtype == "compact_boundary" {
        let meta = obj.get("compactMetadata");
        Routed::Boundary(BoundaryRecord {
            uuid: str_opt(obj, "uuid"),
            parent_uuid: str_opt(obj, "parentUuid"),
            logical_parent_uuid: str_opt(obj, "logicalParentUuid"),
            trigger: meta
                .and_then(|m| m.get("trigger"))
                .and_then(Value::as_str)
                .map(String::from),
            pre_tokens: meta
                .and_then(|m| m.get("preTokens"))
                .and_then(Value::as_i64),
            post_tokens: meta
                .and_then(|m| m.get("postTokens"))
                .and_then(Value::as_i64),
            ts: ts_ms(obj),
            line_no,
        })
    } else {
        // All other system/* are traversable graph nodes with empty FTS.
        message_node(obj, "system", line_no, (String::new(), Vec::new()))
    }
}

fn message_node(obj: &Value, ty: &str, line_no: usize, body: (String, Vec<Block>)) -> Routed {
    let (fts_text, blocks) = body;
    Routed::Message(MessageRecord {
        uuid: str_opt(obj, "uuid"),
        parent_uuid: str_opt(obj, "parentUuid"),
        line_no,
        rec_type: ty.to_string(),
        subtype: str_opt(obj, "subtype"),
        role: obj
            .get("message")
            .and_then(|m| m.get("role"))
            .and_then(Value::as_str)
            .map(String::from),
        ts: ts_ms(obj),
        cwd: str_opt(obj, "cwd"),
        is_sidechain: obj
            .get("isSidechain")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        is_compact_summary: obj
            .get("isCompactSummary")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        blocks,
        fts_text,
    })
}

fn message_content_flatten(obj: &Value) -> (String, Vec<Block>) {
    match obj.get("message").and_then(|m| m.get("content")) {
        Some(c) => flatten_content(c),
        None => (String::new(), Vec::new()),
    }
}

/// Attachment content lives at `attachment.content`. For `file` subtype the text
/// is at `attachment.content.file.content`; we also try a plain string `content`
/// and an `edited_text_file` `snippet`. Anything else → empty FTS (node only).
fn attachment_flatten(obj: &Value) -> (String, Vec<Block>) {
    let att = match obj.get("attachment") {
        Some(a) => a,
        None => return (String::new(), Vec::new()),
    };
    // file: attachment.content.file.content
    if let Some(text) = att
        .get("content")
        .and_then(|c| c.get("file"))
        .and_then(|f| f.get("content"))
        .and_then(Value::as_str)
    {
        return (
            text.to_string(),
            vec![Block::Text {
                text: text.to_string(),
            }],
        );
    }
    // plain string content (skip empty — e.g. hook_success/command_permissions
    // carry an empty content string; store the node with no block)
    if let Some(text) = att.get("content").and_then(Value::as_str) {
        if !text.is_empty() {
            return (
                text.to_string(),
                vec![Block::Text {
                    text: text.to_string(),
                }],
            );
        }
    }
    // edited_text_file: snippet
    if let Some(text) = att.get("snippet").and_then(Value::as_str) {
        return (
            text.to_string(),
            vec![Block::Text {
                text: text.to_string(),
            }],
        );
    }
    (String::new(), Vec::new())
}

fn worktree(obj: &Value, line_no: usize) -> WorktreeRecord {
    let ws = obj.get("worktreeSession");
    let g = |k: &str| {
        ws.and_then(|w| w.get(k))
            .and_then(Value::as_str)
            .map(String::from)
    };
    WorktreeRecord {
        original_cwd: g("originalCwd"),
        worktree_path: g("worktreePath"),
        worktree_name: g("worktreeName"),
        branch: g("worktreeBranch"),
        original_branch: g("originalBranch"),
        original_head: g("originalHeadCommit"),
        continues_session_id: g("sessionId"),
        line_no,
    }
}

fn bridge(obj: &Value, line_no: usize) -> BridgeRecord {
    BridgeRecord {
        bridge_session_id: str_opt(obj, "bridgeSessionId"),
        last_sequence_num: obj.get("lastSequenceNum").and_then(Value::as_i64),
        line_no,
    }
}

fn str_opt(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(String::from)
}

/// Parse the top-level `timestamp` (RFC3339) into epoch milliseconds (UTC).
fn ts_ms(obj: &Value) -> Option<i64> {
    let s = obj.get("timestamp").and_then(Value::as_str)?;
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.timestamp_millis())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn user_message_routes_with_content_and_ts() {
        let line = json!({
            "type":"user","uuid":"u1","parentUuid":null,
            "timestamp":"2026-06-10T14:32:00.000Z","cwd":"/Users/x/proj",
            "message":{"role":"user","content":"where is the bug"}
        });
        match route(&line, 1) {
            Routed::Message(m) => {
                assert_eq!(m.rec_type, "user");
                assert_eq!(m.fts_text, "where is the bug");
                assert_eq!(m.cwd.as_deref(), Some("/Users/x/proj"));
                assert!(m.ts.is_some());
                assert_eq!(m.parent_uuid, None);
            }
            other => panic!("expected message, got {other:?}"),
        }
    }

    #[test]
    fn compact_boundary_routes_to_boundary() {
        let line = json!({
            "type":"system","subtype":"compact_boundary","uuid":"b1","parentUuid":null,
            "logicalParentUuid":"tail-uuid",
            "compactMetadata":{"trigger":"auto","preTokens":150000,"postTokens":3000}
        });
        match route(&line, 5) {
            Routed::Boundary(b) => {
                assert_eq!(b.logical_parent_uuid.as_deref(), Some("tail-uuid"));
                assert_eq!(b.trigger.as_deref(), Some("auto"));
                assert_eq!(b.pre_tokens, Some(150000));
            }
            other => panic!("expected boundary, got {other:?}"),
        }
    }

    #[test]
    fn other_system_is_traversable_node_not_skipped() {
        let line = json!({"type":"system","subtype":"turn_duration","uuid":"s1","parentUuid":"a1"});
        match route(&line, 2) {
            Routed::Message(m) => {
                assert_eq!(m.rec_type, "system");
                assert_eq!(m.parent_uuid.as_deref(), Some("a1"));
                assert_eq!(m.fts_text, "");
            }
            other => panic!("expected traversable message node, got {other:?}"),
        }
    }

    #[test]
    fn compact_summary_flag_is_top_level() {
        let line = json!({
            "type":"user","uuid":"u2","parentUuid":"b1","isCompactSummary":true,
            "message":{"role":"user","content":"This session is being continued..."}
        });
        match route(&line, 6) {
            Routed::Message(m) => assert!(m.is_compact_summary),
            other => panic!("expected message, got {other:?}"),
        }
    }

    #[test]
    fn attachment_file_content_extracted() {
        let line = json!({
            "type":"attachment","uuid":"at1","parentUuid":"u1",
            "attachment":{"type":"file","content":{"type":"text","file":{"content":"fn main() {}"}}}
        });
        match route(&line, 3) {
            Routed::Message(m) => {
                assert_eq!(m.rec_type, "attachment");
                assert!(m.fts_text.contains("fn main"));
            }
            other => panic!("expected message, got {other:?}"),
        }
    }

    #[test]
    fn worktree_state_links_continuation() {
        let line = json!({
            "type":"worktree-state","sessionId":"self",
            "worktreeSession":{"originalCwd":"/Users/x/proj","worktreePath":"/Users/x/proj/.claude/worktrees/api",
                "worktreeName":"api","worktreeBranch":"api","originalBranch":"main",
                "originalHeadCommit":"abc","sessionId":"prior-sib"}
        });
        match route(&line, 2) {
            Routed::Worktree(w) => {
                assert_eq!(w.original_cwd.as_deref(), Some("/Users/x/proj"));
                assert_eq!(w.continues_session_id.as_deref(), Some("prior-sib"));
            }
            other => panic!("expected worktree, got {other:?}"),
        }
    }

    #[test]
    fn attachment_edited_text_file_snippet_extracted() {
        let line = json!({
            "type":"attachment","uuid":"at2","parentUuid":"u1",
            "attachment":{"type":"edited_text_file","snippet":"let x = 42;"}
        });
        match route(&line, 4) {
            Routed::Message(m) => assert!(m.fts_text.contains("let x = 42")),
            other => panic!("expected message, got {other:?}"),
        }
    }

    #[test]
    fn attachment_empty_content_stores_node_without_block() {
        let line = json!({
            "type":"attachment","uuid":"at3","parentUuid":"u1",
            "attachment":{"type":"hook_success","content":""}
        });
        match route(&line, 5) {
            Routed::Message(m) => {
                assert_eq!(m.fts_text, "");
                assert!(m.blocks.is_empty(), "empty content → no block");
            }
            other => panic!("expected message, got {other:?}"),
        }
    }

    #[test]
    fn metadata_lines_skipped() {
        for ty in ["agent-name", "last-prompt", "permission-mode", "mode"] {
            let line = json!({"type": ty});
            assert!(matches!(route(&line, 1), Routed::Skip), "{ty} should skip");
        }
    }
}
