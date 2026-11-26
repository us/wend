//! Rule engine: gitleaks default rules plus wend's own, run the gitleaks way.
//!
//! One Aho-Corasick pass finds which rule keywords occur; only those rules run
//! their regex. A finding is kept unless its entropy is too low for the rule or
//! an allowlist matches. Two checks are not regex rules: base64 strings are
//! decoded and the decoded text scanned, and long mixed-case high-entropy
//! tokens are redacted on their own.

use aho_corasick::{AhoCorasick, AhoCorasickBuilder, MatchKind};
use base64::Engine as _;
use regex::bytes::{Regex, RegexBuilder};
use serde::Deserialize;
use std::collections::BTreeSet;
use std::sync::OnceLock;

/// One redaction. `masked` is at most the first four characters of the secret
/// followed by `...`, so a report never repeats a secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub rule: String,
    pub masked: String,
}

struct Allow {
    whole_match: bool,
    regexes: Vec<Regex>,
    stopwords: Vec<String>,
}

struct Rule {
    id: String,
    re: Regex,
    entropy: Option<f64>,
    secret_group: Option<usize>,
    allow: Vec<Allow>,
    /// Skip the rule when the text contains this marker (PEM tail rule).
    unless_contains: Option<&'static str>,
    /// Drop findings that look like placeholders (`${X}`, `<token>`, `xxxx`).
    skip_placeholders: bool,
}

pub(crate) struct Engine {
    rules: Vec<Rule>,
    ac: AhoCorasick,
    /// Aho-Corasick pattern index -> rule indexes that use that keyword.
    keyword_rules: Vec<Vec<usize>>,
    /// Rules with no keyword run on every text.
    always: Vec<usize>,
    global: Allow,
    base64_re: Regex,
    entropy_re: Regex,
}

#[derive(Deserialize)]
struct GitleaksFile {
    global_allow: AllowJson,
    rules: Vec<RuleJson>,
}

#[derive(Deserialize)]
struct RuleJson {
    id: String,
    regex: String,
    #[serde(default)]
    keywords: Vec<String>,
    entropy: Option<f64>,
    secret_group: Option<usize>,
    #[serde(default)]
    allow: Vec<AllowJson>,
}

#[derive(Deserialize, Default)]
struct AllowJson {
    #[serde(default)]
    target: Option<String>,
    #[serde(default)]
    regexes: Vec<String>,
    #[serde(default)]
    stopwords: Vec<String>,
}

const GITLEAKS: &str = include_str!("gitleaks.json");

/// wend's own rules: (id, regex, keywords, secret group, unless_contains,
/// skip placeholders, entropy floor).
type Custom = (
    &'static str,
    &'static str,
    &'static [&'static str],
    Option<usize>,
    Option<&'static str>,
    bool,
    Option<f64>,
);

const CUSTOM: &[Custom] = &[
    // A private key body, closed or running to the end of the text.
    (
        "private-key-block",
        r"(?s)(-----BEGIN [A-Z ]*PRIVATE KEY-----.*?(?:-----END [A-Z ]*PRIVATE KEY-----|\z))",
        &["private key-----"],
        Some(1),
        None,
        false,
        None,
    ),
    // The tail of a key whose opening marker is in another piece of text.
    (
        "private-key-tail",
        r"(?s)\A(.*?-----END [A-Z ]*PRIVATE KEY-----)",
        &["private key-----"],
        Some(1),
        Some("-----BEGIN"),
        false,
        None,
    ),
    // Bare base64 lines (the middle of a key split across pieces of text).
    (
        "base64-line",
        r"(?m)^([A-Za-z0-9+/=]{64,})\r?$",
        &[],
        Some(1),
        None,
        false,
        // Hex hashes are 64+ chars of this class too; their entropy tops out
        // at 4 bits, random base64 sits near 6.
        Some(4.5),
    ),
    (
        "bearer-token",
        r"(?i)\bbearer\s+([A-Za-z0-9._~+/\-]{20,}=*)",
        &["bearer"],
        Some(1),
        None,
        false,
        None,
    ),
    (
        "url-credentials",
        r"[A-Za-z][A-Za-z0-9+.\-]*://[^\s:/@]+:([^\s@/]{3,})@",
        &["://"],
        Some(1),
        None,
        true,
        None,
    ),
    (
        "secret-assignment",
        r#"(?i)\b[A-Z0-9_]*(?:SECRET|TOKEN|PASSWORD|PASSWD|API_?KEY)[A-Z0-9_]*["']?\s*[:=]\s*["']?([^\s"',;]{8,})"#,
        &["secret", "token", "password", "passwd", "api_key", "apikey"],
        Some(1),
        None,
        true,
        None,
    ),
    (
        "openai-style-key",
        r"\b(sk-(?:proj-|svcacct-|admin-)?[A-Za-z0-9_\-]{20,})",
        &["sk-"],
        Some(1),
        None,
        false,
        None,
    ),
    (
        "anthropic-key",
        r"\b(sk-ant-[a-z0-9]+-[A-Za-z0-9_\-]{20,})",
        &["sk-ant-"],
        Some(1),
        None,
        false,
        None,
    ),
    (
        "supabase-secret-key",
        r"\b(sb_secret_[A-Za-z0-9_\-]{20,})",
        &["sb_secret_"],
        Some(1),
        None,
        false,
        None,
    ),
    (
        "wend-token",
        r"\b(wend_(?:dev|enr)_[A-Za-z0-9]{36})\b",
        &["wend_"],
        Some(1),
        None,
        false,
        None,
    ),
];

fn build(pattern: &str) -> Result<Regex, regex::Error> {
    RegexBuilder::new(pattern)
        .unicode(false)
        .size_limit(64 << 20)
        .build()
        .or_else(|_| RegexBuilder::new(pattern).size_limit(64 << 20).build())
}

fn allow_from(json: AllowJson) -> Allow {
    Allow {
        whole_match: json.target.as_deref() == Some("match"),
        regexes: json
            .regexes
            .iter()
            .map(|r| build(r).unwrap_or_else(|e| panic!("allowlist regex {r:?}: {e}")))
            .collect(),
        stopwords: json.stopwords,
    }
}

impl Engine {
    pub(crate) fn get() -> &'static Engine {
        static ENGINE: OnceLock<Engine> = OnceLock::new();
        ENGINE.get_or_init(Engine::load)
    }

    fn load() -> Engine {
        let file: GitleaksFile =
            serde_json::from_str(GITLEAKS).expect("vendored gitleaks.json must parse");
        let mut rules = Vec::new();
        let mut keywords: Vec<(String, usize)> = Vec::new();
        let mut always = Vec::new();

        for r in file.rules {
            // A rule that does not compile is skipped, never fatal; the test
            // suite asserts that every vendored rule compiles.
            let Ok(re) = build(&r.regex) else { continue };
            let idx = rules.len();
            if r.keywords.is_empty() {
                always.push(idx);
            }
            keywords.extend(r.keywords.iter().map(|k| (k.to_ascii_lowercase(), idx)));
            rules.push(Rule {
                id: r.id,
                re,
                entropy: r.entropy,
                secret_group: r.secret_group,
                allow: r.allow.into_iter().map(allow_from).collect(),
                unless_contains: None,
                skip_placeholders: false,
            });
        }
        for &(id, pattern, kws, group, unless, placeholders, entropy) in CUSTOM {
            let idx = rules.len();
            if kws.is_empty() {
                always.push(idx);
            }
            keywords.extend(kws.iter().map(|k| (k.to_string(), idx)));
            rules.push(Rule {
                id: id.to_string(),
                re: build(pattern).expect("wend scrub rule must compile"),
                entropy,
                secret_group: group,
                allow: Vec::new(),
                unless_contains: unless,
                skip_placeholders: placeholders,
            });
        }

        let mut patterns: Vec<String> = Vec::new();
        let mut keyword_rules: Vec<Vec<usize>> = Vec::new();
        for (kw, idx) in keywords {
            match patterns.iter().position(|p| *p == kw) {
                Some(p) => keyword_rules[p].push(idx),
                None => {
                    patterns.push(kw);
                    keyword_rules.push(vec![idx]);
                }
            }
        }
        let ac = AhoCorasickBuilder::new()
            .ascii_case_insensitive(true)
            .match_kind(MatchKind::Standard)
            .build(&patterns)
            .expect("keyword automaton must build");

        Engine {
            rules,
            ac,
            keyword_rules,
            always,
            global: allow_from(file.global_allow),
            base64_re: build(r"[A-Za-z0-9+/]{40,}={0,2}").unwrap(),
            entropy_re: build(r"[A-Za-z0-9_+/=\-]{32,}").unwrap(),
        }
    }

    /// Number of wend's own rules (tests compare against the vendored count).
    #[cfg(test)]
    pub(crate) fn custom_count() -> usize {
        CUSTOM.len()
    }

    /// Number of loaded rules (for `doctor` and tests).
    pub(crate) fn rule_count(&self) -> usize {
        self.rules.len()
    }

    /// Byte ranges to redact in `text`, with the rule that found each one.
    /// `deep` enables the base64-decode and high-entropy checks.
    fn find(&self, text: &str, deep: bool) -> Vec<(usize, usize, usize)> {
        let bytes = text.as_bytes();
        let mut candidates: BTreeSet<usize> = self.always.iter().copied().collect();
        for m in self.ac.find_overlapping_iter(bytes) {
            candidates.extend(&self.keyword_rules[m.pattern().as_usize()]);
        }

        let mut found = Vec::new();
        for idx in candidates {
            let rule = &self.rules[idx];
            if let Some(marker) = rule.unless_contains {
                if text.contains(marker) {
                    continue;
                }
            }
            for caps in rule.re.captures_iter(bytes) {
                let Some(whole) = caps.get(0) else { continue };
                let secret = match rule.secret_group {
                    Some(g) => caps.get(g),
                    None => (1..caps.len()).find_map(|g| caps.get(g)),
                }
                .unwrap_or(whole);
                if secret.is_empty() || self.allowed(rule, secret.as_bytes(), whole.as_bytes()) {
                    continue;
                }
                found.push((secret.start(), secret.end(), idx));
            }
        }

        if deep {
            self.find_base64(text, &mut found);
            self.find_high_entropy(text, &mut found);
        }
        found
    }

    fn allowed(&self, rule: &Rule, secret: &[u8], whole: &[u8]) -> bool {
        if let Some(min) = rule.entropy {
            if shannon(secret) <= min {
                return true;
            }
        }
        if rule.skip_placeholders && is_placeholder(secret) {
            return true;
        }
        let lower = String::from_utf8_lossy(secret).to_ascii_lowercase();
        let hit = |a: &Allow| {
            let target = if a.whole_match { whole } else { secret };
            a.regexes.iter().any(|r| r.is_match(target))
                || a.stopwords.iter().any(|w| lower.contains(w.as_str()))
        };
        rule.allow.iter().any(hit) || hit(&self.global)
    }

    /// Base64 strings whose decoded text contains a secret are redacted whole.
    fn find_base64(&self, text: &str, found: &mut Vec<(usize, usize, usize)>) {
        for m in self.base64_re.find_iter(text.as_bytes()) {
            let engine = base64::engine::general_purpose::STANDARD;
            let decoded = engine
                .decode(m.as_bytes())
                .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(m.as_bytes()));
            let Ok(decoded) = decoded else { continue };
            let Ok(decoded) = String::from_utf8(decoded) else {
                continue;
            };
            if !self.find(&decoded, false).is_empty() {
                found.push((m.start(), m.end(), usize::MAX - 1));
            }
        }
    }

    /// Long tokens that mix upper case, lower case and digits with high
    /// entropy: vendor keys that no named rule knows.
    fn find_high_entropy(&self, text: &str, found: &mut Vec<(usize, usize, usize)>) {
        for m in self.entropy_re.find_iter(text.as_bytes()) {
            let s = m.as_bytes();
            let hexish = s.iter().all(|b| b.is_ascii_hexdigit() || *b == b'-');
            let upper = s.iter().any(u8::is_ascii_uppercase);
            let lower = s.iter().any(u8::is_ascii_lowercase);
            let digit = s.iter().any(u8::is_ascii_digit);
            // Paths and identifiers are long too; a path separator twice means
            // a path, not a key.
            let slashes = s.iter().filter(|b| **b == b'/').count();
            let id_like = ID_PREFIXES.iter().any(|p| s.starts_with(p.as_bytes()));
            if !hexish && !id_like && upper && lower && digit && slashes < 2 && shannon(s) >= 4.3 {
                found.push((m.start(), m.end(), usize::MAX));
            }
        }
    }

    fn rule_id(&self, idx: usize) -> &str {
        match idx {
            usize::MAX => "high-entropy",
            i if i == usize::MAX - 1 => "base64-encoded-secret",
            i => &self.rules[i].id,
        }
    }

    /// Redact `text`. Returns the new text and the hits, in text order.
    /// `deep` adds the base64-decode and high-entropy checks; callers turn it
    /// off for values that are identifiers or hashes by name.
    /// `None` when nothing was found (no allocation on the common path).
    pub(crate) fn redact(&self, text: &str, deep: bool) -> Option<(String, Vec<Hit>)> {
        let mut ranges = self.find(text, deep);
        if ranges.is_empty() {
            return None;
        }
        ranges.sort_unstable();
        let mut out = String::with_capacity(text.len());
        let mut hits = Vec::new();
        let mut pos = 0;
        let mut i = 0;
        while i < ranges.len() {
            let (start, mut end, idx) = ranges[i];
            let start = floor_char(text, start.max(pos));
            i += 1;
            while i < ranges.len() && ranges[i].0 < end {
                end = end.max(ranges[i].1);
                i += 1;
            }
            let end = ceil_char(text, end);
            if start >= end {
                continue;
            }
            out.push_str(&text[pos..start]);
            let id = self.rule_id(idx);
            out.push_str("[REDACTED:");
            out.push_str(id);
            out.push(']');
            hits.push(Hit {
                rule: id.to_string(),
                masked: mask(&text[start..end]),
            });
            pos = end;
        }
        out.push_str(&text[pos..]);
        Some((out, hits))
    }
}

fn floor_char(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_char(s: &str, mut i: usize) -> usize {
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// Identifiers that look random but are not secrets: agent API message, tool
/// and request ids, and package integrity hashes.
const ID_PREFIXES: &[&str] = &[
    "toolu_",
    "srvtoolu_",
    "msg_",
    "req_",
    "call_",
    "resp_",
    "chatcmpl-",
    "run_",
    "file_",
    "batch_",
    "sha512-",
    "sha384-",
    "sha256-",
    "sha1-",
];

/// At most four leading characters, and never more than a quarter of the
/// secret, so a short secret is not revealed by its own mask.
pub(crate) fn mask(secret: &str) -> String {
    let n = secret.chars().count();
    let head: String = secret.chars().take(4.min(n / 4)).collect();
    format!("{head}...")
}

/// Shannon entropy in bits per byte.
pub(crate) fn shannon(s: &[u8]) -> f64 {
    if s.is_empty() {
        return 0.0;
    }
    let mut counts = [0usize; 256];
    for b in s {
        counts[*b as usize] += 1;
    }
    let n = s.len() as f64;
    counts
        .iter()
        .filter(|c| **c > 0)
        .map(|c| {
            let p = *c as f64 / n;
            -p * p.log2()
        })
        .sum()
}

pub(crate) fn is_placeholder(secret: &[u8]) -> bool {
    let s = String::from_utf8_lossy(secret);
    let lower = s.to_ascii_lowercase();
    let first = s.chars().next();
    s.starts_with("[REDACTED") || s.starts_with("[DROPPED") || s.starts_with("${")
        || s.starts_with("$(")
        || s.starts_with('$')
        || s.starts_with('<')
        || s.starts_with("{{")
        || s.starts_with('%')
        || s.chars().all(|c| Some(c) == first || c == '*')
        // Code, paths and prose, not a value: `std::env::var("X")`,
        // `process.env.X`, `` `src/...` ``, `{...}`.
        || s.starts_with(['`', '{', '[', '/', '.'])
        || (s.contains('/') && shannon(secret) < 4.0)
        || s.contains('(')
        || s.contains("::")
        || lower.starts_with("process.env")
        || lower.starts_with("os.environ")
        || ["changeme", "change_me", "password", "pass", "pwd", "user", "placeholder", "secret", "token", "none", "null"]
            .contains(&lower.as_str())
        || ["your_", "your-", "xxxx", "redacted", "example"]
            .iter()
            .any(|p| lower.starts_with(p) || lower.contains(&format!("_{p}")))
}
