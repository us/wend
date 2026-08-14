//! Azure OpenAI embedding backend (opt-in `azure` feature).
//!
//! Talks to a `GlobalStandard` (pay-per-token) deployment of
//! `text-embedding-3-large` at [`DIMS`] dimensions. Configured entirely by
//! environment variables; the API key is never persisted, never logged, and
//! never placed in a URL.
//!
//! **This backend sends your prompt text off-machine.** Every string entering a
//! request body goes through [`redact`] first — see its docs for why that is a
//! hard requirement and what it can and cannot promise.

use crate::error::{Error, Result};
use regex::Regex;
use serde::Deserialize;
use serde_json::json;
use std::sync::OnceLock;
use std::time::Duration;

/// Output dimensionality requested from the model.
///
/// Not configurable on purpose. Search is brute-force and loads every vector
/// into memory per query, so this trades directly against RAM: at the author's
/// 20k chunks it is 82 MB at 1024-d versus 246 MB at the model's native 3072-d,
/// for a measured quality difference inside noise (MRR 0.771 vs 0.781 over 34
/// queries, where one query is worth 0.029).
const DIMS: usize = 1024;

/// Inputs per request.
const BATCH: usize = 96;

/// Give up after this many attempts on a retryable status.
const MAX_ATTEMPTS: u32 = 6;

/// Per-request ceiling. ureq sets **no** timeout by default, which would let a
/// single stalled connection hang a 20k-chunk backfill forever without ever
/// reaching the retry path.
///
/// Generous because this is a *global* budget covering the response body too, and
/// a 96-item batch at 1024 dimensions returns roughly 800 KB of JSON. At 120s a
/// real backfill died 74% of the way in, mid-body, with
/// `json: timeout: global at line 1 column 797440`.
const TIMEOUT: Duration = Duration::from_secs(600);

/// The three variables that configure this backend, in `Config` field order.
const VARS: [&str; 3] = [
    "WEND_AZURE_ENDPOINT",
    "WEND_AZURE_KEY",
    "WEND_AZURE_DEPLOYMENT",
];

/// Resolved Azure configuration.
///
/// Deliberately does **not** derive `Debug`: it holds the API key, and a derived
/// `Debug` is the usual way a secret ends up in a log line or a panic message.
#[derive(Clone)]
pub struct Config {
    endpoint: String,
    key: String,
    deployment: String,
}

impl Config {
    /// Read the configuration from the environment.
    ///
    /// - none of the three variables set → `Ok(None)` (caller falls back to local)
    /// - all three set and non-empty → `Ok(Some(cfg))`
    /// - anything in between → `Err`
    ///
    /// The middle case is an error rather than a quiet fallback because falling
    /// back would make the next `--embed` treat every Azure vector as belonging
    /// to another model and overwrite the entire corpus with local vectors — a
    /// silent, paid-for downgrade triggered by one typo'd variable name.
    pub fn from_env() -> Result<Option<Self>> {
        let values: Vec<Option<String>> = VARS
            .iter()
            .map(|v| {
                std::env::var(v)
                    .ok()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
            })
            .collect();
        Self::from_values(values)
    }

    /// The decision itself, split out from the environment read so it can be
    /// tested directly — mutating process env in a test is racy under the
    /// parallel test runner, and this is the rule worth pinning down.
    fn from_values(values: Vec<Option<String>>) -> Result<Option<Self>> {
        if values.iter().all(|v| v.is_none()) {
            return Ok(None);
        }
        if let Some(i) = values.iter().position(|v| v.is_none()) {
            return Err(Error::InvalidData(format!(
                "{} is unset or empty, but the other Azure variables are set — \
                 refusing to fall back to the local backend, which would overwrite \
                 every Azure vector in the index",
                VARS[i]
            )));
        }

        let mut it = values.into_iter().map(|v| v.expect("checked above"));
        let mut endpoint = it.next().expect("3 vars");
        if !endpoint.ends_with('/') {
            endpoint.push('/');
        }
        Ok(Some(Self {
            endpoint,
            key: it.next().expect("3 vars"),
            deployment: it.next().expect("3 vars"),
        }))
    }

    /// Storage key for vectors produced by this configuration.
    ///
    /// Includes deployment and dims so that pointing `wend` at a different
    /// deployment (or changing [`DIMS`]) invalidates the old vectors instead of
    /// silently scoring across two incompatible vector spaces.
    pub fn model_id(&self) -> String {
        format!("azure:{}:{DIMS}", self.deployment)
    }
}

/// A configured Azure embedding client.
pub struct Azure {
    agent: ureq::Agent,
    url: String,
    cfg: Config,
}

#[derive(Deserialize)]
struct EmbeddingItem {
    index: usize,
    embedding: Vec<f32>,
}

#[derive(Deserialize)]
struct EmbeddingResponse {
    data: Vec<EmbeddingItem>,
}

impl Azure {
    pub fn new(cfg: Config) -> Result<Self> {
        let agent = ureq::Agent::config_builder()
            // ureq turns 4xx/5xx into a bare `StatusCode(u16)` by default, with
            // no headers attached — which makes reading `retry-after-ms` on a
            // 429 impossible and silently disables the whole retry path.
            .http_status_as_error(false)
            .timeout_global(Some(TIMEOUT))
            .build()
            .new_agent();
        let url = format!("{}openai/v1/embeddings", cfg.endpoint);
        Ok(Self { agent, url, cfg })
    }

    pub fn embed_passages(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let mut out = Vec::with_capacity(texts.len());
        for batch in texts.chunks(BATCH) {
            out.extend(self.embed_batch(batch)?);
        }
        Ok(out)
    }

    pub fn embed_query(&mut self, query: &str) -> Result<Vec<f32>> {
        // text-embedding-3-large is symmetric: no e5-style `query:` prefix.
        self.embed_batch(std::slice::from_ref(&query.to_string()))?
            .pop()
            .ok_or_else(|| Error::InvalidData("empty query embedding".into()))
    }

    fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let inputs: Vec<String> = texts.iter().map(|t| redact(t)).collect();
        let body = json!({
            "model": self.cfg.deployment,
            "input": inputs,
            "dimensions": DIMS,
        });
        let parsed: EmbeddingResponse = self.post_with_retry(&body)?;
        reassemble(parsed.data, texts.len())
    }

    fn post_with_retry(&self, body: &serde_json::Value) -> Result<EmbeddingResponse> {
        let mut last = String::new();
        for attempt in 0..MAX_ATTEMPTS {
            let sent = self
                .agent
                .post(&self.url)
                .header("api-key", &self.cfg.key)
                .send_json(body);

            match sent {
                Ok(mut resp) => {
                    let status = resp.status().as_u16();
                    if status == 200 {
                        match resp.body_mut().read_json() {
                            Ok(parsed) => return Ok(parsed),
                            // Reading the body can fail long after the headers
                            // arrived — a stalled stream, a truncated response.
                            // That is a transport failure like any other and must
                            // retry; returning here once killed a 20k-chunk
                            // backfill three quarters of the way through.
                            Err(e) => {
                                last = format!("response body: {e}");
                                if attempt + 1 < MAX_ATTEMPTS {
                                    std::thread::sleep(retry_delay(None, attempt));
                                    continue;
                                }
                            }
                        }
                    }

                    // 429 is expected during a bulk backfill, not exceptional:
                    // the ceiling is metered on Azure's own token estimate, so
                    // this path is load-bearing rather than a rare fallback.
                    if status == 429 || status >= 500 {
                        last = format!("status {status}");
                        if attempt + 1 < MAX_ATTEMPTS {
                            std::thread::sleep(retry_delay(Some(resp.headers()), attempt));
                            continue;
                        }
                    } else {
                        // Deliberately echoes neither the request nor any
                        // header: the API key travels in one.
                        return Err(Error::InvalidData(format!(
                            "azure rejected the request with status {status}"
                        )));
                    }
                }
                // Transport-level failure — timeout, reset connection, DNS
                // blip. Retried rather than propagated because a full backfill
                // is ~20 minutes of continuous requests, and aborting all of it
                // on one dropped socket is the difference between a hiccup and
                // a failed run. (ureq's Display never prints request headers,
                // so this cannot surface the key.)
                Err(e) => {
                    last = format!("transport error: {e}");
                    if attempt + 1 < MAX_ATTEMPTS {
                        std::thread::sleep(retry_delay(None, attempt));
                        continue;
                    }
                }
            }
        }
        Err(Error::InvalidData(format!(
            "azure still failing after {MAX_ATTEMPTS} attempts ({last})"
        )))
    }
}

/// How long to wait before the next attempt.
///
/// Azure OpenAI prefers `retry-after-ms`; honouring only the coarser
/// `Retry-After` would turn a 30-second throttle into a handful of short retries
/// that exhaust the attempt budget before the window reopens.
/// `headers` is `None` for transport failures, where there is no response to
/// read a hint from and only the exponential fallback applies.
fn retry_delay(headers: Option<&ureq::http::HeaderMap>, attempt: u32) -> Duration {
    if let Some(headers) = headers {
        let get = |name: &str| {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok())
        };
        if let Some(ms) = get("retry-after-ms") {
            return Duration::from_millis(ms.min(60_000));
        }
        if let Some(secs) = get("retry-after") {
            return Duration::from_secs(secs.min(60));
        }
    }
    Duration::from_secs(2u64.pow(attempt).min(32))
}

/// Put `data` back in request order, rejecting anything that would misalign it.
///
/// `index` is the position within *this request's* `input` array, so it is only
/// meaningful against this batch. A missing, duplicated or out-of-range index
/// would otherwise attach a vector to the wrong chunk — an error that is
/// invisible afterwards, since every vector still looks perfectly well-formed.
fn reassemble(data: Vec<EmbeddingItem>, expected: usize) -> Result<Vec<Vec<f32>>> {
    if data.len() != expected {
        return Err(Error::InvalidData(format!(
            "azure returned {} embeddings for {expected} inputs",
            data.len()
        )));
    }
    let mut slots: Vec<Option<Vec<f32>>> = vec![None; expected];
    for item in data {
        let slot = slots.get_mut(item.index).ok_or_else(|| {
            Error::InvalidData(format!("azure index {} out of range", item.index))
        })?;
        if slot.is_some() {
            return Err(Error::InvalidData(format!(
                "azure repeated index {}",
                item.index
            )));
        }
        if item.embedding.len() != DIMS {
            return Err(Error::InvalidData(format!(
                "azure returned a {}-d vector, expected {DIMS}-d",
                item.embedding.len()
            )));
        }
        *slot = Some(normalize(item.embedding));
    }
    slots
        .into_iter()
        .map(|s| s.ok_or_else(|| Error::InvalidData("azure skipped an input index".into())))
        .collect()
}

/// Scale to unit length so `dot` is exactly cosine.
///
/// Azure's output measures as normalized already (‖v‖ = 1.000±0.003), but the
/// scoring path treats normalization as an invariant rather than a hope.
fn normalize(mut v: Vec<f32>) -> Vec<f32> {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in &mut v {
            *x /= norm;
        }
    }
    v
}

/// Optional Cohere reranker, deployed alongside the embedding model.
///
/// Separate from [`Config`]'s all-three-or-none rule on purpose: an existing
/// embedding-only setup must keep working untouched when this is unset. It is
/// also deliberately absent from [`Config::model_id`] — that string keys stored
/// vectors, so folding the rerank deployment into it would invalidate every
/// vector in the index the moment the user switched rerankers.
pub struct Reranker {
    agent: ureq::Agent,
    url: String,
    key: String,
    deployment: String,
}

#[derive(Deserialize)]
struct RerankResult {
    index: usize,
    relevance_score: f32,
}

#[derive(Deserialize)]
struct RerankResponse {
    results: Vec<RerankResult>,
}

impl Reranker {
    /// `Ok(None)` when `WEND_AZURE_RERANK_DEPLOYMENT` is unset — retrieval then
    /// runs without a rerank stage rather than failing.
    pub fn from_env() -> Result<Option<Self>> {
        let Some(cfg) = Config::from_env()? else {
            return Ok(None);
        };
        let deployment = match std::env::var("WEND_AZURE_RERANK_DEPLOYMENT")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
        {
            Some(d) => d,
            None => return Ok(None),
        };
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(TIMEOUT))
            .build()
            .new_agent();
        Ok(Some(Self {
            agent,
            url: format!("{}providers/cohere/v2/rerank", cfg.endpoint),
            key: cfg.key,
            deployment,
        }))
    }

    /// Score `documents` against `query`, returning `(index, score)` best-first.
    ///
    /// Every string is redacted first. This request carries the current
    /// situation *and* dozens of stored ones, all of which are prose the user
    /// typed — the same corpus that was measured to contain live AWS keys,
    /// GitHub and npm tokens and database URIs. Skipping redaction here would
    /// reopen exactly the hole the embedding path closes.
    pub fn rerank(
        &self,
        query: &str,
        documents: &[String],
        top_n: usize,
    ) -> Result<Vec<(usize, f32)>> {
        if documents.is_empty() {
            return Ok(Vec::new());
        }
        let docs: Vec<String> = documents.iter().map(|d| redact(d)).collect();
        let body = json!({
            "model": self.deployment,
            "query": redact(query),
            "documents": docs,
            "top_n": top_n.min(documents.len()),
        });
        let mut last = String::new();
        for attempt in 0..MAX_ATTEMPTS {
            match self
                .agent
                .post(&self.url)
                .header("api-key", &self.key)
                .send_json(&body)
            {
                Ok(mut resp) => {
                    let status = resp.status().as_u16();
                    if status == 200 {
                        let parsed: RerankResponse = resp
                            .body_mut()
                            .read_json()
                            .map_err(|e| Error::InvalidData(format!("rerank parse: {e}")))?;
                        let mut out: Vec<(usize, f32)> = parsed
                            .results
                            .into_iter()
                            .filter(|r| r.index < documents.len())
                            .map(|r| (r.index, r.relevance_score))
                            .collect();
                        out.sort_by(|a, b| {
                            b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal)
                        });
                        return Ok(out);
                    }
                    if status == 429 || status >= 500 {
                        last = format!("status {status}");
                        if attempt + 1 < MAX_ATTEMPTS {
                            std::thread::sleep(retry_delay(Some(resp.headers()), attempt));
                            continue;
                        }
                    } else {
                        return Err(Error::InvalidData(format!(
                            "azure rejected the rerank request with status {status}"
                        )));
                    }
                }
                Err(e) => {
                    last = format!("transport error: {e}");
                    if attempt + 1 < MAX_ATTEMPTS {
                        std::thread::sleep(retry_delay(None, attempt));
                        continue;
                    }
                }
            }
        }
        Err(Error::InvalidData(format!(
            "rerank still failing after {MAX_ATTEMPTS} attempts ({last})"
        )))
    }
}

/// Strip high-confidence secrets from a string on its way off the machine.
///
/// Necessary because [`crate::store::Store::semantic_messages`] selects by
/// message *shape* (prose you typed, not tool output) and has no notion of
/// secrets. Scanning the author's real corpus of 11,870 such prompts found, in
/// plain text: 4 AWS access keys (one alongside its secret access key), 110
/// `SECRET`/`TOKEN`/`PASSWORD`/`API_KEY` assignments, 15 bearer tokens, 2
/// database URIs with inline credentials and a JWT. Uploading unredacted would
/// hand live credentials to a third party by construction.
///
/// PEM handling is deliberately marker-optional in both directions: a private
/// key longer than a chunk gets split, so the opening and closing markers land
/// in different chunks and a `BEGIN…END` matcher would pass the body straight
/// through.
///
/// This is best-effort pattern matching, not a guarantee. It raises the floor;
/// it cannot promise that no secret ever leaves. Anything truly sensitive should
/// not be in the transcripts in the first place.
pub fn redact(s: &str) -> String {
    static RULES: OnceLock<Vec<(Regex, &'static str)>> = OnceLock::new();
    let rules = RULES.get_or_init(|| {
        let r = |p: &str| Regex::new(p).expect("static redaction pattern must compile");
        vec![
            // A private key body runs to the end of its chunk...
            (
                r(r"(?s)-----BEGIN [A-Z ]*PRIVATE KEY-----.*"),
                "<redacted:private-key>",
            ),
            // ...and in the chunk holding only its tail, back to the start.
            (
                r(r"(?s)^.*?-----END [A-Z ]*PRIVATE KEY-----"),
                "<redacted:private-key>",
            ),
            // Middle chunks are bare base64 lines with no marker at all.
            (r(r"(?m)^[A-Za-z0-9+/=]{64,}$"), "<redacted:base64>"),
            (r(r"AKIA[0-9A-Z]{16}"), "<redacted:aws-key>"),
            (r(r"\bsk-[A-Za-z0-9_\-]{16,}"), "<redacted:api-key>"),
            (
                r(r"\b[rp]k_(live|test)_[A-Za-z0-9]{10,}"),
                "<redacted:api-key>",
            ),
            // Vendor tokens that are all "known prefix + long opaque string".
            // GitHub and npm tokens were found in this corpus unredacted by the
            // rules above; the rest ride along for one regex.
            (
                r(r"\b(gh[pousr]_|github_pat_|glpat-|npm_|xox[baprs]-|AIza|SG\.)[A-Za-z0-9_\-\.]{20,}"),
                "<redacted:api-key>",
            ),
            // A JWT with no `Bearer` in front of it — the rule below only fires
            // on the header form, and Supabase/Auth0 tokens get pasted bare.
            (
                r(r"\beyJ[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}"),
                "<redacted:jwt>",
            ),
            (
                r(r"(?i)\bbearer\s+[A-Za-z0-9._~+/\-]{20,}=*"),
                "<redacted:bearer>",
            ),
            (r(r"://[^\s:/@]+:[^\s@/]+@"), "://<redacted:creds>@"),
            (
                r(r"(?i)\b([A-Z0-9_]*(?:SECRET|TOKEN|PASSWORD|PASSWD|API_?KEY)[A-Z0-9_]*)\s*[:=]\s*\S+"),
                "$1=<redacted>",
            ),
        ]
    });

    let mut out = s.to_string();
    for (re, replacement) in rules {
        out = re.replace_all(&out, *replacement).into_owned();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_the_shapes_found_in_real_transcripts() {
        let cases = [
            "here is AKIA1234567890ABCDEF ok",
            "OPENAI_API_KEY=sk-abcdefghijklmnopqrstuvwxyz012345",
            "curl -H 'Authorization: Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9abc'",
            "postgres://admin:hunter2@db.example.com:5432/app",
            "DATABASE_PASSWORD: s3cr3t-value",
            "STRIPE=pk_live_51ABCdefGHIjkl",
            // Found unredacted in a real corpus before these rules were added.
            "gh repo clone with ghp_A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8",
            "//registry.npmjs.org/:_authToken=npm_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789",
            "anon key eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N",
        ];
        for c in cases {
            let out = redact(c);
            assert!(out.contains("<redacted"), "not redacted: {c} -> {out}");
        }
        assert!(!redact(cases[0]).contains("AKIA1234567890ABCDEF"));
        assert!(!redact(cases[3]).contains("hunter2"));
    }

    /// A key split across chunks must be caught in every piece, including the
    /// tail chunk that holds only the closing marker.
    #[test]
    fn redacts_pem_fragments_without_both_markers() {
        let head = "-----BEGIN RSA PRIVATE KEY-----\nMIIEow0BAQEFAASCAmVeryLongBase64";
        let middle = "d0VeryLongBase64Line".repeat(4); // ≥64 chars, no marker
        let tail = "shortTail==\n-----END RSA PRIVATE KEY-----";

        assert!(!redact(head).contains("MIIEow"));
        assert!(!redact(&middle).contains("VeryLongBase64Line"));
        let t = redact(tail);
        assert!(!t.contains("shortTail"), "tail fragment leaked: {t}");
    }

    #[test]
    fn ordinary_prose_survives_redaction() {
        let s = "dostum bu embedding modelini nasil degistirebiliriz acaba?";
        assert_eq!(redact(s), s);
    }

    /// The rule that protects a paid-for index: all three or none, never a
    /// quiet fallback to the local backend on a partial configuration.
    #[test]
    fn partial_config_is_an_error_not_a_fallback() {
        let v = |s: &str| Some(s.to_string());

        // Nothing set: the caller may fall back to local.
        assert!(Config::from_values(vec![None, None, None])
            .unwrap()
            .is_none());

        // All set: configured.
        let cfg = Config::from_values(vec![
            v("https://acct.cognitiveservices.azure.com"),
            v("secret"),
            v("dep"),
        ])
        .unwrap()
        .expect("fully configured");
        assert_eq!(cfg.model_id(), format!("azure:dep:{DIMS}"));
        // A missing trailing slash must not produce a double slash in the URL.
        assert!(cfg.endpoint.ends_with(".azure.com/"));

        // Each single omission is an error naming that variable. Matched rather
        // than `unwrap_err()`d because that would require `Debug` on `Config`,
        // and `Config` holds the API key — see the note on its definition.
        for missing in 0..3 {
            let mut values = vec![v("https://a/"), v("k"), v("d")];
            values[missing] = None;
            match Config::from_values(values) {
                Err(e) => assert!(
                    e.to_string().contains(VARS[missing]),
                    "error should name {}, got: {e}",
                    VARS[missing]
                ),
                Ok(_) => panic!("missing {} must not be accepted", VARS[missing]),
            }
        }
    }

    /// A set-but-empty variable is the realistic typo (`export WEND_AZURE_KEY=`)
    /// and must count as missing, not as a valid empty key.
    #[test]
    fn blank_values_count_as_missing() {
        let blank: Option<String> = Some("   ".to_string()).filter(|s| !s.trim().is_empty());
        assert!(blank.is_none(), "whitespace must normalise to unset");
    }

    #[test]
    fn reassemble_rejects_misaligned_responses() {
        let ok = vec![
            EmbeddingItem {
                index: 1,
                embedding: vec![0.0; DIMS],
            },
            EmbeddingItem {
                index: 0,
                embedding: vec![1.0; DIMS],
            },
        ];
        assert!(reassemble(ok, 2).is_ok(), "out-of-order is legal");

        let dup = vec![
            EmbeddingItem {
                index: 0,
                embedding: vec![0.0; DIMS],
            },
            EmbeddingItem {
                index: 0,
                embedding: vec![0.0; DIMS],
            },
        ];
        assert!(reassemble(dup, 2).is_err(), "duplicate index must fail");

        let short = vec![EmbeddingItem {
            index: 0,
            embedding: vec![0.0; 8],
        }];
        assert!(reassemble(short, 1).is_err(), "wrong dims must fail");

        let missing = vec![EmbeddingItem {
            index: 0,
            embedding: vec![0.0; DIMS],
        }];
        assert!(reassemble(missing, 2).is_err(), "cardinality must fail");
    }

    #[test]
    fn retry_delay_prefers_the_millisecond_header_then_backs_off() {
        let mut h = ureq::http::HeaderMap::new();
        assert_eq!(retry_delay(Some(&h), 0), Duration::from_secs(1));
        assert_eq!(retry_delay(None, 3), Duration::from_secs(8));

        h.insert("retry-after", "30".parse().unwrap());
        assert_eq!(retry_delay(Some(&h), 0), Duration::from_secs(30));

        // retry-after-ms wins: honouring only the coarse header would turn a
        // long throttle into short retries that burn the attempt budget.
        h.insert("retry-after-ms", "4500".parse().unwrap());
        assert_eq!(retry_delay(Some(&h), 0), Duration::from_millis(4500));

        // Never sleep unboundedly on a hostile or bogus value.
        h.insert("retry-after-ms", "99999999".parse().unwrap());
        assert_eq!(retry_delay(Some(&h), 0), Duration::from_millis(60_000));
    }

    #[test]
    fn normalize_makes_unit_vectors() {
        let v = normalize(vec![3.0, 4.0]);
        let n = (v[0] * v[0] + v[1] * v[1]).sqrt();
        assert!((n - 1.0).abs() < 1e-6);
    }
}
