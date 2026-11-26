//! Secret scrubbing for transcripts before they leave the machine.
//!
//! Two layers:
//! - [`text`] redacts secrets in a string with the gitleaks default rules plus
//!   wend's own (see `engine.rs`), a base64-decode check and a high-entropy
//!   check. Matches become `[REDACTED:<rule>]`.
//! - [`Scrubber`] works on transcript lines (Claude Code or Codex JSONL). It
//!   drops the content of tool calls that read or write secrets files, print
//!   the environment, or ran wend itself (every copy of the result, including
//!   Claude Code's `toolUseResult`, repeated request copies and Codex exec
//!   events), replaces images, keeps signed and encrypted fields
//!   byte-identical, and redacts every other string. It never drops a line, so
//!   a scrubbed session still resumes.
//!
//! This is best-effort pattern matching. It raises the floor; it cannot
//! promise that no secret ever leaves.

mod engine;
mod policy;

pub use engine::Hit;

use serde_json::{Map, Value};
use std::collections::HashSet;

/// Redact secrets in plain text.
pub fn text(s: &str) -> (String, Vec<Hit>) {
    engine::Engine::get()
        .redact(s, true)
        .unwrap_or_else(|| (s.to_string(), Vec::new()))
}

/// How many rules are loaded (gitleaks + wend).
pub fn rule_count() -> usize {
    engine::Engine::get().rule_count()
}

/// Version of the scrubbing rules, sent with every upload. The server
/// re-scrubs every upload with its own (newest) rules before indexing, so it
/// does not need to branch on this yet; it is stored for later re-scrubs.
pub const RULESET_VERSION: u32 = 1;

pub const SENSITIVE_READ: &str = "[DROPPED:sensitive-read]";
pub const SENSITIVE_WRITE: &str = "[DROPPED:sensitive-write]";
pub const WEND_OUTPUT: &str = "[DROPPED:wend-output]";

/// Which agent wrote the transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Claude,
    Codex,
}

/// Scrubs one transcript file line by line, from its first line. Keep one per
/// file: a decision made on a tool call (sensitive, or ran wend) applies to
/// its result on a later line.
#[derive(Debug)]
pub struct Scrubber {
    format: Format,
    sensitive: HashSet<String>,
    wend: HashSet<String>,
    /// A user-typed `!command` was sensitive (or ran wend); its output
    /// arrives on a later line.
    bang: Option<&'static str>,
}

/// Input fields that carry file content in Claude Code's write tools.
const WRITE_FIELDS: &[&str] = &["content", "new_string", "old_string", "edits", "new_source"];
/// Fields that carry a call's output in Codex records.
const OUTPUT_FIELDS: &[&str] = &[
    "output",
    "stdout",
    "stderr",
    "aggregated_output",
    "formatted_output",
    "chunk",
    "result",
];
/// Content fields blanked on an object that names a secrets file.
const FILE_CONTENT_FIELDS: &[&str] = &[
    "content",
    "text",
    "stdout",
    "stderr",
    "originalFile",
    "structuredPatch",
    "newString",
    "oldString",
];

/// What a tool call touches: paths and commands, never file bodies.
#[derive(Default)]
struct Targets {
    paths: Vec<String>,
    commands: Vec<String>,
}

impl Targets {
    /// Commands are judged up to their first heredoc: the body is content,
    /// and a redirect into a secrets file sits before the `<<`.
    fn sensitive(&self) -> bool {
        self.paths.iter().any(|p| policy::is_sensitive(p))
            || self.commands.iter().any(|c| {
                policy::is_sensitive(before_heredoc(c))
                    || patch_paths(c).iter().any(|p| policy::is_sensitive(p))
            })
    }

    fn runs_wend(&self) -> bool {
        self.commands.iter().any(|c| policy::runs_wend(c))
    }

    /// A sensitive command that writes (redirect, heredoc, tee) carries the
    /// secret in the command itself.
    fn writes(&self) -> bool {
        self.commands.iter().any(|c| {
            let head = before_heredoc(c)
                .replace("2>&1", "")
                .replace(">/dev/null", "")
                .replace("> /dev/null", "");
            head.contains('>') || c.contains("<<") || head.contains("tee ")
        })
    }

    fn is_empty(&self) -> bool {
        self.paths.is_empty() && self.commands.is_empty()
    }
}

impl Scrubber {
    pub fn new(format: Format) -> Self {
        Scrubber {
            format,
            sensitive: HashSet::new(),
            wend: HashSet::new(),
            bang: None,
        }
    }

    /// Scrub one line. Returns the line to keep (the exact input bytes when
    /// nothing was found) and the hits. Never drops the line.
    pub fn line(&mut self, line: &str) -> (String, Vec<Hit>) {
        let Ok(mut value) = serde_json::from_str::<Value>(line) else {
            return text(line);
        };
        let mut hits = Vec::new();
        let mut changed = match self.format {
            Format::Claude => self.claude(&mut value, &mut hits),
            Format::Codex => self.codex(&mut value),
        };
        changed |= walk(&mut value, None, &mut hits);
        if !changed {
            return (line.to_string(), hits);
        }
        let out = serde_json::to_string(&value).unwrap_or_else(|_| text(line).0);
        (out, hits)
    }

    /// Claude Code. Tool calls, results and thinking blocks are handled
    /// wherever they appear: `api-request-blob` lines repeat earlier messages.
    fn claude(&mut self, v: &mut Value, hits: &mut Vec<Hit>) -> bool {
        let mut tool_result_tag = None;
        let mut changed = self.claude_bang(v);
        changed |= self.claude_visit(v, hits, &mut tool_result_tag);
        if let Some(tag) = tool_result_tag {
            if let Some(obj) = v.as_object_mut() {
                if obj.contains_key("toolUseResult") {
                    obj.insert("toolUseResult".into(), Value::String(tag.into()));
                }
            }
        }
        changed
    }

    /// `!command` typed by the user: `<bash-input>` on one line, its
    /// `<bash-stdout>` / `<bash-stderr>` on the next.
    fn claude_bang(&mut self, v: &mut Value) -> bool {
        let Some(content) = v.pointer_mut("/message/content") else {
            return false;
        };
        let Some(msg) = content.as_str() else {
            return false;
        };
        if let Some(rest) = msg.split("<bash-input>").nth(1) {
            let cmd = rest.split("</bash-input>").next().unwrap_or_default();
            self.bang = if policy::is_sensitive(before_heredoc(cmd)) {
                Some(SENSITIVE_READ)
            } else if policy::runs_wend(cmd) {
                Some(WEND_OUTPUT)
            } else {
                None
            };
            return false;
        }
        if msg.contains("<bash-stdout>") || msg.contains("<bash-stderr>") {
            if let Some(tag) = self.bang.take() {
                *content = Value::String(tag.into());
                return true;
            }
        }
        false
    }

    fn claude_visit(
        &mut self,
        v: &mut Value,
        hits: &mut Vec<Hit>,
        tag: &mut Option<&'static str>,
    ) -> bool {
        match v {
            Value::Object(obj) => {
                let mut changed = false;
                match obj.get("type").and_then(Value::as_str) {
                    Some("tool_use") => changed |= self.claude_tool_use(obj),
                    Some("tool_result") => {
                        if let Some(t) =
                            self.tag_for(obj.get("tool_use_id").and_then(Value::as_str))
                        {
                            obj.insert("content".into(), Value::String(t.into()));
                            *tag = Some(t);
                            return true;
                        }
                    }
                    _ => {}
                }
                changed |= blank_sensitive_file(obj);
                for x in obj.values_mut() {
                    changed |= self.claude_visit(x, hits, tag);
                }
                changed
            }
            Value::Array(items) => {
                let mut changed = false;
                let before = items.len();
                items.retain(|b| !dirty_thinking(b, hits));
                if items.len() != before {
                    changed = true;
                    if items.is_empty() {
                        items.push(placeholder_text("[thinking omitted]"));
                    }
                }
                for x in items.iter_mut() {
                    changed |= self.claude_visit(x, hits, tag);
                }
                changed
            }
            _ => false,
        }
    }

    fn claude_tool_use(&mut self, block: &mut Map<String, Value>) -> bool {
        let id = block.get("id").and_then(Value::as_str).map(str::to_string);
        let is_glob = block.get("name").and_then(Value::as_str) == Some("Glob");
        let Some(Value::Object(input)) = block.get_mut("input") else {
            return false;
        };
        let mut t = Targets::default();
        call_targets(input, &mut t);
        if is_glob {
            // Glob's `pattern` is a path pattern (Grep's is a regex, not a path).
            if let Some(p) = input.get("pattern").and_then(Value::as_str) {
                t.paths.push(p.to_string());
            }
        }
        let sensitive = t.sensitive();
        if let Some(id) = id {
            if sensitive {
                self.sensitive.insert(id);
            } else if t.runs_wend() {
                self.wend.insert(id);
            }
        }
        if !sensitive {
            return false;
        }
        let mut changed = blank_fields(input, WRITE_FIELDS, SENSITIVE_WRITE);
        if t.writes() {
            changed |= blank_fields(input, &["command"], SENSITIVE_WRITE);
        }
        changed
    }

    /// Codex: a call carries a `call_id` (or an item `id`) with its command,
    /// arguments, action, patch or file changes below it; outputs come back
    /// with the same id, sometimes more than once.
    fn codex(&mut self, v: &mut Value) -> bool {
        let mut changed = false;
        visit_objects(v, &mut |obj| changed |= self.codex_object(obj));
        changed
    }

    fn codex_object(&mut self, obj: &mut Map<String, Value>) -> bool {
        let Some(id) = obj
            .get("call_id")
            .or_else(|| obj.get("id"))
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            return false;
        };
        let mut changed = false;

        let mut t = Targets::default();
        call_targets(obj, &mut t);
        if !t.is_empty() {
            if t.sensitive() {
                self.sensitive.insert(id.clone());
                changed |= blank_fields(obj, &["input", "changes"], SENSITIVE_WRITE);
                if t.writes() {
                    changed |= blank_fields(
                        obj,
                        &["command", "cmd", "arguments", "action"],
                        SENSITIVE_WRITE,
                    );
                }
            } else if t.runs_wend() {
                self.wend.insert(id.clone());
            }
        }

        if let Some(tag) = self.tag_for(Some(&id)) {
            changed |= blank_fields(obj, OUTPUT_FIELDS, tag);
        }
        changed
    }

    fn tag_for(&self, id: Option<&str>) -> Option<&'static str> {
        let id = id?;
        if self.sensitive.contains(id) {
            Some(SENSITIVE_READ)
        } else if self.wend.contains(id) {
            Some(WEND_OUTPUT)
        } else {
            None
        }
    }
}

/// Collect a call's paths and commands from its input or call object. Only
/// fields that name a target are read; file bodies are never classified.
fn call_targets(m: &Map<String, Value>, t: &mut Targets) {
    for (k, x) in m {
        match (k.as_str(), x) {
            ("file_path" | "path" | "notebook_path" | "glob", _) => push_strings(x, &mut t.paths),
            ("command" | "cmd", _) => t.commands.extend(command_text(x)),
            ("arguments", Value::String(s)) => {
                if let Ok(Value::Object(parsed)) = serde_json::from_str::<Value>(s) {
                    call_targets(&parsed, t);
                }
            }
            ("arguments" | "action" | "invocation" | "params" | "input", Value::Object(inner)) => {
                call_targets(inner, t)
            }
            // apply_patch: only the file headers name targets.
            ("input", Value::String(s)) => t.paths.extend(patch_paths(s)),
            ("changes", Value::Object(files)) => t.paths.extend(files.keys().cloned()),
            ("changes", Value::Array(items)) => t.paths.extend(
                items
                    .iter()
                    .filter_map(|i| i.get("path").and_then(Value::as_str).map(str::to_string)),
            ),
            _ => {}
        }
    }
}

fn push_strings(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) => out.push(s.clone()),
        Value::Array(a) => a.iter().for_each(|x| push_strings(x, out)),
        _ => {}
    }
}

/// A command as shell text. An argv like `["/bin/zsh", "-lc", "script"]` is
/// unwrapped to `script`.
fn command_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Array(items) => {
            let argv: Vec<&str> = items.iter().filter_map(Value::as_str).collect();
            if argv.len() >= 3 && policy::is_shell(argv[0]) && policy::is_c_flag(argv[1]) {
                Some(argv[2..].join(" "))
            } else {
                Some(argv.join(" "))
            }
        }
        _ => None,
    }
}

fn before_heredoc(command: &str) -> &str {
    command.split("<<").next().unwrap_or(command)
}

fn patch_paths(patch: &str) -> Vec<String> {
    const HEADERS: [&str; 4] = [
        "*** Add File: ",
        "*** Update File: ",
        "*** Delete File: ",
        "*** Move to: ",
    ];
    patch
        .lines()
        .filter_map(|l| HEADERS.iter().find_map(|h| l.strip_prefix(h)))
        .map(|p| p.trim().to_string())
        .collect()
}

/// A thinking block whose text holds a secret. Signed, so it cannot be
/// edited; it is removed whole.
fn dirty_thinking(block: &Value, hits: &mut Vec<Hit>) -> bool {
    if block.get("type").and_then(Value::as_str) != Some("thinking") {
        return false;
    }
    let thinking = block
        .get("thinking")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let (_, found) = text(thinking);
    if found.is_empty() {
        return false;
    }
    hits.extend(found);
    true
}

/// An object that names a secrets file (an @-mentioned `.env`, a hook or
/// tool payload) loses its content fields.
fn blank_sensitive_file(obj: &mut Map<String, Value>) -> bool {
    let names_secret_file = ["filePath", "file_path", "path", "filename"]
        .iter()
        .filter_map(|k| obj.get(*k).and_then(Value::as_str))
        .any(policy::is_sensitive);
    names_secret_file && blank_fields(obj, FILE_CONTENT_FIELDS, SENSITIVE_READ)
}

/// Redact every string leaf except opaque ones (signatures, encrypted content,
/// image and redacted-thinking data), and replace images wherever they are.
/// A key that names a secret redacts its value outright.
fn walk(v: &mut Value, key: Option<&str>, hits: &mut Vec<Hit>) -> bool {
    match v {
        Value::String(s) => {
            // Inline binaries (Claude Code keeps read images as `base64`).
            if key == Some("base64") && s.len() > 100 {
                *s = "[binary omitted]".into();
                return true;
            }
            if key.is_some_and(is_secret_key) && looks_like_value(s) {
                hits.push(Hit {
                    rule: "secret-key-field".into(),
                    masked: engine::mask(s),
                });
                *s = "[REDACTED:secret-key-field]".into();
                return true;
            }
            // Identifier and hash fields skip the entropy checks (named rules
            // still run on them).
            let deep = !key.is_some_and(is_id_key);
            match engine::Engine::get().redact(s, deep) {
                Some((out, found)) => {
                    hits.extend(found);
                    *s = out;
                    true
                }
                None => false,
            }
        }
        Value::Array(items) => {
            let mut changed = false;
            for item in items {
                changed |= walk(item, key, hits);
            }
            changed
        }
        Value::Object(map) => {
            let kind = map.get("type").and_then(Value::as_str).map(str::to_string);
            match kind.as_deref() {
                Some("image") => {
                    *v = placeholder_text("[image omitted]");
                    return true;
                }
                Some("input_image") if map.contains_key("image_url") => {
                    map.insert("image_url".into(), Value::String("[image omitted]".into()));
                    return true;
                }
                _ => {}
            }
            let mut changed = false;
            for (k, item) in map.iter_mut() {
                if is_opaque(kind.as_deref(), k) {
                    continue;
                }
                changed |= walk(item, Some(k), hits);
            }
            changed
        }
        _ => false,
    }
}

fn is_opaque(kind: Option<&str>, key: &str) -> bool {
    match key {
        "encrypted_content" => true,
        "signature" => kind == Some("thinking"),
        "data" => matches!(kind, Some("base64" | "redacted_thinking")),
        _ => false,
    }
}

fn is_id_key(key: &str) -> bool {
    let words = key_words(key);
    words.last().is_some_and(|w| {
        matches!(
            w.as_str(),
            "id" | "uuid" | "hash" | "digest" | "integrity" | "etag" | "checksum" | "sha"
        )
    })
}

/// A key whose last word names a secret: `api_key`, `clientSecret`,
/// `auth_token`, `password`. `tokenizer`, `secretName` and `password_policy`
/// name something else.
fn is_secret_key(key: &str) -> bool {
    let words = key_words(key);
    let Some(last) = words.last() else {
        return false;
    };
    let prev = words.len().checked_sub(2).map(|i| words[i].as_str());
    matches!(
        last.as_str(),
        "secret"
            | "token"
            | "password"
            | "passwd"
            | "pwd"
            | "credential"
            | "credentials"
            | "apikey"
            | "auth"
            | "authorization"
    ) || (last == "key"
        && matches!(
            prev,
            Some("api" | "private" | "secret" | "access" | "signing" | "encryption")
        ))
}

/// Split a key into lowercase words on `_`, `-`, `.` and camelCase.
fn key_words(key: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut prev_lower = false;
    for c in key.chars() {
        if matches!(c, '_' | '-' | '.' | ' ') {
            if !cur.is_empty() {
                words.push(std::mem::take(&mut cur));
            }
            prev_lower = false;
            continue;
        }
        if c.is_ascii_uppercase() && prev_lower {
            words.push(std::mem::take(&mut cur));
        }
        prev_lower = c.is_ascii_lowercase() || c.is_ascii_digit();
        cur.push(c.to_ascii_lowercase());
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    words
}

/// A value worth redacting under a secret-named key: 6+ non-space
/// characters, not a placeholder, not a URL.
fn looks_like_value(s: &str) -> bool {
    let t = s.trim();
    t.len() >= 6
        && !t.contains(char::is_whitespace)
        && !engine::is_placeholder(t.as_bytes())
        && !t.starts_with("http")
}

fn placeholder_text(t: &str) -> Value {
    serde_json::json!({ "type": "text", "text": t })
}

fn blank_fields(obj: &mut Map<String, Value>, fields: &[&str], tag: &str) -> bool {
    let mut changed = false;
    for f in fields {
        if let Some(slot) = obj.get_mut(*f) {
            // Already dropped (by this or another rule): leave it.
            if !slot.as_str().is_some_and(|v| v.starts_with("[DROPPED:")) {
                *slot = Value::String(tag.into());
                changed = true;
            }
        }
    }
    changed
}

fn visit_objects(v: &mut Value, f: &mut dyn FnMut(&mut Map<String, Value>)) {
    match v {
        Value::Object(m) => {
            f(m);
            for x in m.values_mut() {
                visit_objects(x, f);
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|x| visit_objects(x, f)),
        _ => {}
    }
}

#[cfg(test)]
mod tests;
