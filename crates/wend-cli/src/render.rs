//! Render stored content blocks into a readable terminal transcript.

use wend_core::model::Block;

/// Render a message's blocks into a human-readable string (trailing newline per block).
pub fn render_blocks(blocks: &[Block]) -> String {
    let mut out = String::new();
    for b in blocks {
        render_block(b, &mut out);
    }
    out
}

fn render_block(b: &Block, out: &mut String) {
    match b {
        Block::Text { text } => {
            out.push_str(text.trim_end());
            out.push('\n');
        }
        Block::Thinking { text } => {
            let t = truncate(text, 200);
            if !t.is_empty() {
                out.push_str("  💭 ");
                out.push_str(&t);
                out.push('\n');
            }
        }
        Block::ToolUse { name, input, .. } => {
            out.push_str(&format!("  → {name}({})\n", summarize_input(input)));
        }
        Block::ToolResult {
            is_error, content, ..
        } => {
            out.push_str(if *is_error {
                "  ← [error]\n"
            } else {
                "  ← [result]\n"
            });
            for c in content {
                let mut inner = String::new();
                render_block(c, &mut inner);
                for line in inner.lines() {
                    out.push_str("    ");
                    out.push_str(line);
                    out.push('\n');
                }
            }
        }
        Block::Image {
            media_type,
            byte_len,
        } => {
            out.push_str(&format!("  [image {media_type} ~{byte_len}B]\n"));
        }
        Block::ToolReference { tool_name } => {
            out.push_str(&format!("  [tool: {tool_name}]\n"));
        }
    }
}

fn summarize_input(input: &serde_json::Value) -> String {
    match input {
        serde_json::Value::Null => String::new(),
        other => truncate(&other.to_string(), 120),
    }
}

fn truncate(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}…")
    }
}
