//! Content-block flattening.
//!
//! `message.content` is either a plain string or an array of typed blocks. We
//! produce two outputs: the FTS body text (image/thinking excluded) and the
//! normalized [`Block`] list (always kept, for rendering). `tool_result.content`
//! is recursed.

use crate::model::Block;
use serde_json::Value;

/// Flatten a `content` value (string or array) into `(fts_text, blocks)`.
pub(crate) fn flatten_content(content: &Value) -> (String, Vec<Block>) {
    match content {
        Value::String(s) => {
            let blocks = if s.is_empty() {
                Vec::new()
            } else {
                vec![Block::Text { text: s.clone() }]
            };
            (s.clone(), blocks)
        }
        Value::Array(arr) => {
            let mut fts_parts: Vec<String> = Vec::new();
            let mut blocks: Vec<Block> = Vec::new();
            for item in arr {
                if let Some((part, block)) = flatten_block(item) {
                    if !part.is_empty() {
                        fts_parts.push(part);
                    }
                    blocks.push(block);
                }
            }
            (fts_parts.join("\n"), blocks)
        }
        _ => (String::new(), Vec::new()),
    }
}

fn flatten_block(item: &Value) -> Option<(String, Block)> {
    let ty = item.get("type")?.as_str()?;
    match ty {
        "text" => {
            let text = str_field(item, "text");
            Some((text.clone(), Block::Text { text }))
        }
        "thinking" => {
            // Excluded from FTS by default; kept in the normalized form for render.
            let text = item
                .get("thinking")
                .or_else(|| item.get("text"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            Some((String::new(), Block::Thinking { text }))
        }
        "tool_use" => {
            let id = str_field(item, "id");
            let name = str_field(item, "name");
            let input = item.get("input").cloned().unwrap_or(Value::Null);
            let scalars = collect_string_scalars(&input);
            let fts = if scalars.is_empty() {
                name.clone()
            } else {
                format!("{name} {scalars}")
            };
            Some((fts, Block::ToolUse { id, name, input }))
        }
        "tool_result" => {
            let tool_use_id = item
                .get("tool_use_id")
                .and_then(Value::as_str)
                .map(String::from);
            let is_error = item
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let inner = item.get("content").unwrap_or(&Value::Null);
            let (fts, content) = flatten_content(inner);
            Some((
                fts,
                Block::ToolResult {
                    tool_use_id,
                    is_error,
                    content,
                },
            ))
        }
        "image" => {
            // Never store base64 — only a descriptor; excluded from FTS.
            let media_type = item
                .get("source")
                .and_then(|s| s.get("media_type"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            // Approximate decoded size from the base64 length (≈ len * 3/4);
            // we never store the base64 itself.
            let byte_len = item
                .get("source")
                .and_then(|s| s.get("data"))
                .and_then(Value::as_str)
                .map(|d| d.len() * 3 / 4)
                .unwrap_or(0);
            Some((
                String::new(),
                Block::Image {
                    media_type,
                    byte_len,
                },
            ))
        }
        "tool_reference" => {
            let tool_name = str_field(item, "tool_name");
            Some((tool_name.clone(), Block::ToolReference { tool_name }))
        }
        _ => None,
    }
}

fn str_field(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

/// Recursively collect string leaves from a JSON value (used to make tool inputs
/// searchable without dumping structure).
fn collect_string_scalars(v: &Value) -> String {
    let mut out: Vec<&str> = Vec::new();
    collect_into(v, &mut out);
    out.join(" ")
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
    use serde_json::json;

    #[test]
    fn string_content() {
        let (fts, blocks) = flatten_content(&json!("hello world"));
        assert_eq!(fts, "hello world");
        assert_eq!(
            blocks,
            vec![Block::Text {
                text: "hello world".into()
            }]
        );
    }

    #[test]
    fn thinking_excluded_from_fts_but_kept() {
        let (fts, blocks) =
            flatten_content(&json!([{"type":"thinking","thinking":"secret reasoning"}]));
        assert_eq!(fts, "");
        assert_eq!(blocks.len(), 1);
        assert!(matches!(blocks[0], Block::Thinking { .. }));
    }

    #[test]
    fn image_never_in_fts_and_no_base64() {
        let big = "A".repeat(5000);
        let (fts, blocks) = flatten_content(&json!([
            {"type":"image","source":{"media_type":"image/png","data": big}}
        ]));
        assert_eq!(fts, "");
        match &blocks[0] {
            Block::Image {
                media_type,
                byte_len,
            } => {
                assert_eq!(media_type, "image/png");
                assert_eq!(*byte_len, 3750); // 5000 base64 chars ≈ 3750 decoded bytes
            }
            other => panic!("expected image, got {other:?}"),
        }
    }

    #[test]
    fn assistant_mixed_blocks() {
        let (fts, blocks) = flatten_content(&json!([
            {"type":"thinking","thinking":"hmm"},
            {"type":"text","text":"Here is the fix"},
            {"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls -la"}}
        ]));
        assert!(fts.contains("Here is the fix"));
        assert!(fts.contains("Bash"));
        assert!(fts.contains("ls -la"));
        assert!(!fts.contains("hmm"));
        assert_eq!(blocks.len(), 3);
    }

    #[test]
    fn nested_tool_result_with_reference() {
        let (fts, blocks) = flatten_content(&json!([
            {"type":"tool_result","tool_use_id":"t1","content":[
                {"type":"text","text":"output line"},
                {"type":"tool_reference","tool_name":"SendMessage"},
                {"type":"image","source":{"media_type":"image/png","data":"xxxx"}}
            ]}
        ]));
        assert!(fts.contains("output line"));
        assert!(fts.contains("SendMessage"));
        match &blocks[0] {
            Block::ToolResult { content, .. } => assert_eq!(content.len(), 3),
            other => panic!("expected tool_result, got {other:?}"),
        }
    }
}
