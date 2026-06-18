//! Domain types produced by the parser.
//!
//! A Claude Code `.jsonl` line is routed into one of a few record kinds. Most
//! line types (user/assistant/attachment/progress and all non-boundary
//! `system/*`) become a [`MessageRecord`] graph node — even when they carry no
//! searchable text — because the recovery DFS must be able to traverse through
//! them. Compaction boundaries, worktree state, bridges, and title metadata get
//! their own records.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A normalized, renderable content block. Stored as `content_json` so `show`
/// and `export` reproduce the original transcript (base64 image bytes are never
/// stored — only a descriptor).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Block {
    Text {
        text: String,
    },
    Thinking {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: Option<String>,
        is_error: bool,
        content: Vec<Block>,
    },
    Image {
        media_type: String,
        byte_len: usize,
    },
    ToolReference {
        tool_name: String,
    },
}

/// A graph node stored in the `messages` table. `fts_text` may be empty (then it
/// is not FTS-indexed) but the row is always stored so the conversation graph
/// stays connected.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageRecord {
    pub uuid: Option<String>,
    pub parent_uuid: Option<String>,
    pub line_no: usize,
    /// Top-level `type` (e.g. "user", "assistant", "attachment", "progress", "system").
    pub rec_type: String,
    /// `subtype` where present (e.g. for `system` lines).
    pub subtype: Option<String>,
    /// `message.role` for user/assistant lines.
    pub role: Option<String>,
    /// Epoch milliseconds (UTC), if a timestamp was present and parseable.
    pub ts: Option<i64>,
    pub cwd: Option<String>,
    pub is_sidechain: bool,
    pub is_compact_summary: bool,
    pub blocks: Vec<Block>,
    pub fts_text: String,
}

/// A `system` + `subtype:"compact_boundary"` line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoundaryRecord {
    pub uuid: Option<String>,
    pub parent_uuid: Option<String>,
    pub logical_parent_uuid: Option<String>,
    pub trigger: Option<String>,
    pub pre_tokens: Option<i64>,
    pub post_tokens: Option<i64>,
    pub ts: Option<i64>,
    pub line_no: usize,
}

/// A `worktree-state` line. Links a worktree session to the session it continues
/// and (via `original_cwd`) to its parent repo.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorktreeRecord {
    pub original_cwd: Option<String>,
    pub worktree_path: Option<String>,
    pub worktree_name: Option<String>,
    pub branch: Option<String>,
    pub original_branch: Option<String>,
    pub original_head: Option<String>,
    pub continues_session_id: Option<String>,
    pub line_no: usize,
}

/// A `bridge-session` line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BridgeRecord {
    pub bridge_session_id: Option<String>,
    pub last_sequence_num: Option<i64>,
    pub line_no: usize,
}

/// Session-level title metadata (`ai-title` latest-wins, or user-set `custom-title`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TitleUpdate {
    Ai(String),
    Custom(String),
}

/// The result of routing one parsed line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Routed {
    Message(MessageRecord),
    Boundary(BoundaryRecord),
    Worktree(WorktreeRecord),
    Bridge(BridgeRecord),
    Title(TitleUpdate),
    /// Recognized but not stored (e.g. `agent-name`, `last-prompt`, `permission-mode`).
    Skip,
}
