//! wend cloud client: login, sync (scrub + upload), and cloud-backed search,
//! show and resume. Everything here runs on the user's machine, so it is open
//! source: anyone can read exactly what leaves the machine (see `sync`).

use std::collections::{BTreeMap, HashMap};
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use wend_core::scrub::{Format, Scrubber, RULESET_VERSION};
use wend_core::store::{MessageRow, SearchHit, SessionRef};
use wend_core::{config, index};

/// The server refuses bigger uploads.
const MAX_UPLOAD: usize = 64 << 20;
/// The server refuses transcripts that decompress to more than this.
const MAX_TRANSCRIPT: i64 = 1 << 30;
/// Raw bytes scrubbed in parallel before uploading (bounds memory: each file
/// is held about three times while it is prepared).
const BATCH_BYTES: i64 = 512 << 20;
/// Largest transcript accepted back from the server (zip-bomb guard).
const MAX_REHYDRATE: u64 = 1 << 30;

// ---------- credentials and HTTP ----------

#[derive(Serialize, Deserialize)]
struct Credentials {
    server: String,
    device_id: i64,
    token: String,
}

pub struct Client {
    server: String,
    token: String,
    agent: ureq::Agent,
    /// Uploads stream up to 64 MiB; they get the server's 30-minute window.
    upload_agent: ureq::Agent,
}

fn agent() -> ureq::Agent {
    agent_with_timeout(Duration::from_secs(300))
}

fn upload_agent() -> ureq::Agent {
    agent_with_timeout(Duration::from_secs(30 * 60))
}

fn agent_with_timeout(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        // A token must never follow a redirect to another host.
        .max_redirects(0)
        .http_status_as_error(false)
        .timeout_global(Some(timeout))
        .build()
        .into()
}

/// `https://` anywhere, plain `http://` only to this machine. Parsed, so
/// `http://localhost:80@evil.example` is seen for what it is.
fn check_server(url: &str) -> Result<String> {
    let url = url.trim().trim_end_matches('/').to_string();
    let uri: ureq::http::Uri = url
        .parse()
        .map_err(|_| anyhow!("refusing {url}: not a valid URL"))?;
    let authority = uri
        .authority()
        .ok_or_else(|| anyhow!("refusing {url}: no host"))?;
    if authority.as_str().contains('@') {
        bail!("refusing {url}: credentials in the URL");
    }
    let host = authority.host();
    let local = matches!(host, "localhost" | "127.0.0.1" | "[::1]" | "::1");
    match uri.scheme_str() {
        Some("https") => Ok(url),
        Some("http") if local => Ok(url),
        _ => bail!("refusing {url}: wend cloud needs https (plain http only for localhost)"),
    }
}

/// Server and token from the environment, if any. They only work as a pair:
/// `WEND_SERVER` alone could redirect the stored token, `WEND_TOKEN` alone
/// would have no server to go to.
fn from_env(token: Option<String>, server: Option<String>) -> Result<Option<(String, String)>> {
    match (token, server) {
        (Some(token), Some(server)) => Ok(Some((server, token))),
        (None, None) => Ok(None),
        _ => bail!("WEND_TOKEN and WEND_SERVER must be set together"),
    }
}

impl Client {
    /// The logged-in client. `WEND_TOKEN` (+ optional `WEND_SERVER`) override
    /// the credentials file; `WEND_SERVER` alone is refused, so an environment
    /// variable can never send the stored token to another server.
    pub fn load() -> Result<Client> {
        let env = |k: &str| {
            std::env::var(k)
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let (server, token) = match from_env(env("WEND_TOKEN"), env("WEND_SERVER"))? {
            Some(pair) => pair,
            None => {
                let path = config::credentials_path()?;
                let raw = std::fs::read_to_string(&path).map_err(|_| {
                    anyhow!("not logged in to wend cloud: run `wend login --enroll <token>`")
                })?;
                let c: Credentials = serde_json::from_str(&raw)
                    .with_context(|| format!("corrupt {}", path.display()))?;
                (c.server, c.token)
            }
        };
        Ok(Client {
            server: check_server(&server)?,
            token,
            agent: agent(),
            upload_agent: upload_agent(),
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.server)
    }

    fn auth(&self) -> String {
        format!("Bearer {}", self.token)
    }

    fn unreachable(&self, e: ureq::Error) -> anyhow::Error {
        anyhow!("wend cloud unreachable ({}): {e}", self.server)
    }

    /// GET a JSON body; non-2xx becomes an error carrying the server message.
    fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<T> {
        let mut req = self
            .agent
            .get(self.url(path))
            .header("Authorization", self.auth());
        for (k, v) in query {
            req = req.query(*k, *v);
        }
        let resp = req.call().map_err(|e| self.unreachable(e))?;
        json_or_error(resp)
    }
}

/// Decode a 2xx JSON body, or turn the server's `{error}` into an error.
fn json_or_error<T: serde::de::DeserializeOwned>(
    mut resp: ureq::http::Response<ureq::Body>,
) -> Result<T> {
    let status = resp.status();
    let body: serde_json::Value = resp
        .body_mut()
        .with_config()
        .limit(64 << 20)
        .read_json()
        .unwrap_or(serde_json::Value::Null);
    if !status.is_success() {
        return Err(ServerError {
            status: status.as_u16(),
            body,
        }
        .into());
    }
    Ok(serde_json::from_value(body)?)
}

#[derive(Debug)]
pub struct ServerError {
    pub status: u16,
    pub body: serde_json::Value,
}

impl std::fmt::Display for ServerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.status {
            401 => write!(f, "wend cloud: not authorized (token revoked or invalid); run `wend login --enroll <token>`"),
            404 => write!(f, "wend cloud: not found"),
            _ => write!(
                f,
                "wend cloud: HTTP {}: {}",
                self.status,
                self.body.get("error").and_then(|e| e.as_str()).unwrap_or("no details")
            ),
        }
    }
}

impl std::error::Error for ServerError {}

// ---------- login / logout ----------

pub fn login(
    enroll: Option<String>,
    server: Option<String>,
    device_name: Option<String>,
) -> Result<()> {
    let token = match enroll.filter(|t| !t.is_empty()) {
        Some(t) => t,
        None => match std::env::var("WEND_ENROLL_TOKEN") {
            Ok(t) if !t.is_empty() => t,
            _ => {
                if std::io::stdin().is_terminal() {
                    eprint!("enrollment token: ");
                    std::io::stderr().flush().ok();
                }
                let mut line = String::new();
                std::io::stdin().read_line(&mut line)?;
                line.trim().to_string()
            }
        },
    };
    if token.is_empty() {
        bail!("no enrollment token given (argument, WEND_ENROLL_TOKEN, or stdin)");
    }
    let server =
        check_server(server.as_deref().ok_or_else(|| {
            anyhow!("pass --server <url> (the wend cloud address you were given)")
        })?)?;
    let device_name = device_name.unwrap_or_else(default_device_name);
    let resp = agent()
        .post(format!("{server}/v1/enroll"))
        .send_json(serde_json::json!({ "token": token, "device_name": device_name }))
        .map_err(|e| anyhow!("wend cloud unreachable ({server}): {e}"))?;
    #[derive(Deserialize)]
    struct Enrolled {
        device_id: i64,
        token: String,
    }
    let enrolled: Enrolled = json_or_error(resp)?;
    let path = config::credentials_path()?;
    write_private(
        &path,
        serde_json::to_string_pretty(&Credentials {
            server: server.clone(),
            device_id: enrolled.device_id,
            token: enrolled.token,
        })?
        .as_bytes(),
    )?;
    println!(
        "logged in to {server} as device {} ({device_name})",
        enrolled.device_id
    );
    println!("next: `wend sync --dry-run` to see what would be uploaded");
    Ok(())
}

fn default_device_name() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .or_else(|| {
            std::process::Command::new("hostname")
                .output()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "device".into())
}

/// Create or replace a file readable only by this user.
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Revoke this machine's stored device and forget it. Only the credentials
/// file counts here (never `WEND_TOKEN`), so logout cannot revoke a
/// different device than the one it deletes.
pub fn logout() -> Result<()> {
    let path = config::credentials_path()?;
    let Ok(raw) = std::fs::read_to_string(&path) else {
        println!("not logged in");
        return Ok(());
    };
    match serde_json::from_str::<Credentials>(&raw) {
        Ok(c) => {
            let revoked = check_server(&c.server).and_then(|server| {
                agent()
                    .post(format!("{server}/v1/logout"))
                    .header("Authorization", format!("Bearer {}", c.token))
                    .send_empty()
                    .map_err(|e| anyhow!("could not reach {server}: {e}"))
            });
            match revoked {
                Ok(r) if r.status().is_success() || r.status() == 401 => {}
                Ok(r) => eprintln!("warning: the server did not revoke this device (HTTP {}); revoke it from the device list", r.status()),
                Err(e) => eprintln!("warning: {e:#}; the device token is still valid on the server, revoke it from the device list"),
            }
        }
        Err(_) => eprintln!("warning: unreadable credentials file; the device token may still be valid on the server"),
    }
    std::fs::remove_file(&path)?;
    println!("logged out");
    Ok(())
}

// ---------- sync ----------

#[derive(Deserialize)]
struct Manifest {
    files: Vec<ManifestFile>,
    tombstones: Vec<String>,
}

#[derive(Deserialize)]
struct ManifestFile {
    original_path: String,
    received_hash: String,
    mtime: i64,
    size: i64,
}

struct Candidate {
    path: PathBuf,
    source: &'static str,
    mtime: i64,
    size: i64,
}

/// A file scrubbed and ready to upload.
struct Prepared {
    candidate: Candidate,
    hash: String,
    raw_len: usize,
    compressed: Vec<u8>,
    hits: BTreeMap<String, Vec<String>>,
}

fn mtime_ns(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or_default()
}

fn candidates() -> Result<Vec<Candidate>> {
    let mut out = Vec::new();
    let projects = config::projects_dir()?;
    let codex = config::codex_sessions_dir()?;
    let found = index::discover_top_level(&projects)?
        .into_iter()
        .map(|p| (p, "claude"))
        .chain(
            index::discover_codex(&codex)?
                .into_iter()
                .map(|p| (p, "codex")),
        );
    for (path, source) in found {
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        out.push(Candidate {
            mtime: mtime_ns(&meta),
            size: meta.len() as i64,
            path,
            source,
        });
    }
    Ok(out)
}

/// Scrub a whole transcript file line by line (from its first line, so the
/// scrubber sees every tool call before its result), hash and compress it.
fn prepare(c: Candidate) -> Result<Prepared> {
    let mut raw = String::new();
    std::fs::File::open(&c.path)?.read_to_string(&mut raw)?;
    let format = if c.source == "codex" {
        Format::Codex
    } else {
        Format::Claude
    };
    let mut scrubber = Scrubber::new(format);
    let mut out = String::with_capacity(raw.len());
    let mut hits: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for line in raw.split_inclusive('\n') {
        let (body, nl) = match line.strip_suffix('\n') {
            Some(b) => (b, "\n"),
            None => (line, ""),
        };
        let (clean, found) = scrubber.line(body);
        for h in found {
            hits.entry(h.rule).or_default().push(h.masked);
        }
        out.push_str(&clean);
        out.push_str(nl);
    }
    let hash = format!("{:x}", Sha256::digest(out.as_bytes()));
    let compressed = zstd::encode_all(out.as_bytes(), 3)?;
    Ok(Prepared {
        candidate: c,
        hash,
        raw_len: out.len(),
        compressed,
        hits,
    })
}

pub fn sync(dry_run: bool, yes: bool) -> Result<()> {
    let start = Instant::now();
    let client = Client::load()?;
    // A dry run only peeks: it must not count as a sync on the server.
    let peek: &[(&str, &str)] = if dry_run { &[("peek", "1")] } else { &[] };
    let manifest: Manifest = client.get_json("/v1/manifest", peek)?;
    let known: HashMap<&str, &ManifestFile> = manifest
        .files
        .iter()
        .map(|f| (f.original_path.as_str(), f))
        .collect();
    let tombstoned: std::collections::HashSet<&str> =
        manifest.tombstones.iter().map(String::as_str).collect();

    let all = candidates()?;
    let scanned = all.len();
    let mut skipped_deleted = 0;
    let mut unchanged = 0;
    let mut work = Vec::new();
    for c in all {
        let key = c.path.to_string_lossy();
        if tombstoned.contains(key.as_ref()) {
            skipped_deleted += 1;
            continue;
        }
        match known.get(key.as_ref()) {
            Some(m) if m.mtime == c.mtime && m.size == c.size => unchanged += 1,
            _ => work.push(c),
        }
    }

    let first_sync = manifest.files.is_empty();
    if first_sync && !dry_run && !yes {
        println!(
            "first sync: {} transcripts would be scrubbed and uploaded.",
            work.len()
        );
        if !std::io::stdin().is_terminal() {
            bail!("first sync needs confirmation: run `wend sync --dry-run` to review, then `wend sync --yes`");
        }
        eprint!("review with `wend sync --dry-run` first, or type `yes` to upload now: ");
        std::io::stderr().flush().ok();
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if answer.trim() != "yes" {
            println!("nothing uploaded");
            return Ok(());
        }
    }

    let mut totals: BTreeMap<String, usize> = BTreeMap::new();
    let (mut uploaded, mut stat_only, mut too_big, mut failed) = (0usize, 0usize, 0usize, 0usize);
    let (mut bytes_raw, mut bytes_sent) = (0usize, 0usize);
    let (work, oversized): (Vec<Candidate>, Vec<Candidate>) =
        work.into_iter().partition(|c| c.size <= MAX_TRANSCRIPT);
    for c in &oversized {
        too_big += 1;
        eprintln!("warning: {} is over 1 GB; not uploaded", c.path.display());
    }
    // Scrub in parallel, upload in order; one batch (by bytes) in memory at a time.
    let mut iter = work.into_iter().peekable();
    while iter.peek().is_some() {
        let mut chunk: Vec<Candidate> = Vec::new();
        let mut budget = 0i64;
        while let Some(c) = iter.next_if(|c| chunk.is_empty() || budget + c.size <= BATCH_BYTES) {
            budget += c.size;
            chunk.push(c);
        }
        let prepared: Vec<Result<Prepared>> = chunk.into_par_iter().map(prepare).collect();
        for p in prepared {
            let p = match p {
                Ok(p) => p,
                Err(e) => {
                    failed += 1;
                    eprintln!("warning: skipped a transcript: {e:#}");
                    continue;
                }
            };
            for (rule, masks) in &p.hits {
                *totals.entry(rule.clone()).or_default() += masks.len();
            }
            let path = p.candidate.path.to_string_lossy().into_owned();
            if dry_run {
                if !p.hits.is_empty() {
                    println!("{path}");
                    for (rule, masks) in &p.hits {
                        let shown: Vec<&str> = masks.iter().take(3).map(String::as_str).collect();
                        println!("  {:>4}  {rule}  {}", masks.len(), shown.join(" "));
                    }
                }
                continue;
            }
            if known
                .get(path.as_str())
                .is_some_and(|m| m.received_hash == p.hash)
            {
                match client.patch_stat(&path, &p) {
                    Ok(()) => {
                        stat_only += 1;
                        continue;
                    }
                    // Hash no longer stored (replaced or deleted): upload it.
                    Err(e) if server_status(&e) == Some(409) => {}
                    Err(e) => return Err(e),
                }
            }
            if p.compressed.len() > MAX_UPLOAD {
                too_big += 1;
                eprintln!(
                    "warning: {path} is too large to upload ({} MB compressed)",
                    p.compressed.len() >> 20
                );
                continue;
            }
            match client.upload(&path, &p) {
                Ok(()) => {
                    uploaded += 1;
                    bytes_raw += p.raw_len;
                    bytes_sent += p.compressed.len();
                }
                Err(e) if server_status(&e) == Some(409) => skipped_deleted += 1,
                // One bad file must not block every file after it; stop only
                // on auth or transport trouble.
                Err(e) if server_status(&e).is_some_and(|s| s != 401) => {
                    failed += 1;
                    eprintln!("warning: {path} not uploaded: {e:#}");
                }
                Err(e) => return Err(e),
            }
        }
    }

    let verb = if dry_run { "would redact" } else { "redacted" };
    let redactions: Vec<String> = totals.iter().map(|(r, n)| format!("{r} {n}")).collect();
    println!(
        "{scanned} transcripts: {uploaded} uploaded, {stat_only} unchanged content, {unchanged} unchanged, {skipped_deleted} deleted in the cloud, {too_big} too large, {failed} unreadable · {:.1} MB scrubbed -> {:.1} MB sent · {:.1}s",
        bytes_raw as f64 / 1e6,
        bytes_sent as f64 / 1e6,
        start.elapsed().as_secs_f64()
    );
    println!(
        "{verb}: {}",
        if redactions.is_empty() {
            "nothing".into()
        } else {
            redactions.join(", ")
        }
    );
    if dry_run {
        println!("dry run: nothing uploaded");
    }
    if failed > 0 {
        bail!("{failed} transcript(s) could not be uploaded (see warnings above)");
    }
    Ok(())
}

fn server_status(e: &anyhow::Error) -> Option<u16> {
    e.downcast_ref::<ServerError>().map(|s| s.status)
}

impl Client {
    fn upload(&self, path: &str, p: &Prepared) -> Result<()> {
        let resp = self
            .upload_agent
            .put(self.url("/v1/uploads"))
            .header("Authorization", self.auth())
            .header("x-wend-source", p.candidate.source)
            .header("x-wend-path", percent_encode(path))
            .header("x-wend-hash", &p.hash)
            .header("x-wend-mtime", p.candidate.mtime.to_string())
            .header("x-wend-size", p.candidate.size.to_string())
            .header("x-wend-ruleset", RULESET_VERSION.to_string())
            .send(&p.compressed[..])
            .map_err(|e| self.unreachable(e))?;
        json_or_error::<serde_json::Value>(resp).map(|_| ())
    }

    fn patch_stat(&self, path: &str, p: &Prepared) -> Result<()> {
        let resp = self
            .agent
            .patch(self.url("/v1/uploads/stat"))
            .header("Authorization", self.auth())
            .send_json(serde_json::json!({
                "original_path": path,
                "mtime": p.candidate.mtime,
                "size": p.candidate.size,
                "hash": p.hash,
            }))
            .map_err(|e| self.unreachable(e))?;
        if resp.status().is_success() {
            Ok(())
        } else {
            json_or_error::<serde_json::Value>(resp).map(|_| ())
        }
    }
}

/// Header values must be printable ASCII: encode every other byte, and `%`,
/// as `%XX` (the server decodes it).
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if (0x21..=0x7e).contains(&b) && b != b'%' {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

// ---------- cloud search / show / resume ----------

#[derive(Deserialize)]
struct Status {
    sessions: i64,
    last_sync_at: Option<i64>,
}

/// `cloud: 812 sessions (Claude Code, Codex), last sync 2h ago`
pub fn header(client: &Client) -> Result<String> {
    let s: Status = client.get_json("/v1/status", &[])?;
    let ago = match s.last_sync_at {
        Some(t) => {
            let now = std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(t);
            human_age(now - t)
        }
        None => "never".into(),
    };
    Ok(format!(
        "cloud: {} sessions (Claude Code, Codex), last sync {ago}",
        s.sessions
    ))
}

fn human_age(secs: i64) -> String {
    match secs {
        s if s < 90 => "just now".into(),
        s if s < 3600 => format!("{}m ago", s / 60),
        s if s < 86_400 => format!("{}h ago", s / 3600),
        s => format!("{}d ago", s / 86_400),
    }
}

pub fn search(
    client: &Client,
    query: &str,
    limit: usize,
    role: Option<&str>,
    source: Option<&str>,
) -> Result<Vec<SearchHit>> {
    #[derive(Deserialize)]
    struct Hits {
        hits: Vec<SearchHit>,
    }
    let limit = limit.to_string();
    let mut q = vec![("q", query), ("limit", limit.as_str())];
    if let Some(r) = role {
        q.push(("role", r));
    }
    if let Some(s) = source {
        q.push(("source", s));
    }
    Ok(client.get_json::<Hits>("/v1/search", &q)?.hits)
}

pub struct CloudSession {
    pub session: SessionRef,
    pub messages: Vec<MessageRow>,
}

/// Fetch a session by id or prefix. An ambiguous prefix lists candidates.
pub fn session(client: &Client, id: &str) -> Result<CloudSession> {
    #[derive(Deserialize)]
    struct Shown {
        session: SessionRef,
        messages: Vec<MessageRow>,
    }
    match client.get_json::<Shown>(&format!("/v1/sessions/{}", url_segment(id)?), &[]) {
        Ok(s) => Ok(CloudSession {
            session: s.session,
            messages: s.messages,
        }),
        Err(e) => match e.downcast_ref::<ServerError>() {
            Some(se) if se.status == 409 => {
                eprintln!("ambiguous id '{id}' in the cloud:");
                for c in se.body["candidates"].as_array().into_iter().flatten() {
                    eprintln!(
                        "  {}  [{}]  {}",
                        c["session_id"].as_str().unwrap_or("?"),
                        c["source"].as_str().unwrap_or("?"),
                        c["title"].as_str().unwrap_or("")
                    );
                }
                Err(e.context("disambiguate with a longer id prefix"))
            }
            Some(se) if se.status == 404 => {
                Err(e.context(format!("no cloud session matching '{id}'")))
            }
            _ => Err(e),
        },
    }
}

fn url_segment(id: &str) -> Result<&str> {
    if id.len() >= 4
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        Ok(id)
    } else {
        bail!("session id must be 4+ characters of letters, digits, '-' or '_'")
    }
}

/// Resume a cloud session: if its transcript is not on this machine, download
/// it (scrubbed) into the agent's own store, then print the resume command.
pub fn resume(
    client: &Client,
    id: &str,
    cwd_override: Option<PathBuf>,
) -> Result<(SessionRef, Option<PathBuf>)> {
    let s = session(client, id)?.session;
    let written = match s.source.as_str() {
        "claude" => {
            let cwd = cwd_override
                .or_else(|| s.project_path.as_ref().map(PathBuf::from))
                .ok_or_else(|| anyhow!("the session has no project directory; pass --cwd <dir>"))?;
            if !cwd.is_dir() {
                bail!(
                    "project directory {} does not exist here; pass --cwd <your checkout>",
                    cwd.display()
                );
            }
            // Claude Code keys its folder on the canonical path.
            let cwd = std::fs::canonicalize(&cwd)?;
            let target = claude_target(&config::projects_dir()?, &cwd, &s.session_id)?;
            if target.exists() {
                None
            } else {
                let (raw, _) = client.raw(&s.session_id)?;
                write_new(&config::projects_dir()?, &target, &raw)?;
                Some(target)
            }
        }
        "codex" => {
            let root = config::codex_sessions_dir()?;
            let on_disk = index::discover_codex(&root)?
                .into_iter()
                .any(|p| p.to_string_lossy().contains(&s.session_id));
            if on_disk {
                None
            } else {
                let (raw, file_name) = client.raw(&s.session_id)?;
                let target = codex_target(&root, &s.session_id, &raw, file_name.as_deref())?;
                write_new(&root, &target, &raw)?;
                Some(target)
            }
        }
        other => bail!("resuming {other} sessions from the cloud is not supported yet"),
    };
    Ok((s, written))
}

impl Client {
    /// The scrubbed transcript, decompressed with a size cap, and the
    /// original file name (no directories) as the server recorded it.
    fn raw(&self, session_id: &str) -> Result<(Vec<u8>, Option<String>)> {
        let mut resp = self
            .agent
            .get(self.url(&format!("/v1/sessions/{}/raw", url_segment(session_id)?)))
            .header("Authorization", self.auth())
            .call()
            .map_err(|e| self.unreachable(e))?;
        if !resp.status().is_success() {
            return json_or_error::<serde_json::Value>(resp).map(|_| (Vec::new(), None));
        }
        let file_name = resp
            .headers()
            .get("x-wend-file")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let compressed = resp
            .body_mut()
            .with_config()
            .limit(256 << 20)
            .read_to_vec()?;
        let mut out = Vec::new();
        zstd::stream::read::Decoder::new(&compressed[..])?
            .take(MAX_REHYDRATE + 1)
            .read_to_end(&mut out)?;
        if out.len() as u64 > MAX_REHYDRATE {
            bail!("downloaded transcript is over the 1 GB cap");
        }
        Ok((out, file_name))
    }
}

/// Claude Code's folder name for a working directory: every character that
/// is not an ASCII letter or digit becomes `-`.
pub fn encode_cwd(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

fn is_uuid(s: &str) -> bool {
    s.len() == 36
        && s.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// `<projects>/<encoded cwd>/<uuid>.jsonl`, built only from local data and a
/// validated id, so the server cannot steer where the file lands.
pub fn claude_target(projects: &Path, cwd: &Path, session_id: &str) -> Result<PathBuf> {
    if !is_uuid(session_id) {
        bail!("refusing to write session '{session_id}': not a UUID");
    }
    Ok(projects
        .join(encode_cwd(cwd))
        .join(format!("{session_id}.jsonl")))
}

/// `<sessions>/YYYY/MM/DD/rollout-YYYY-MM-DDTHH-MM-SS-<uuid>.jsonl`.
/// Codex names rollouts in local time, so the original file name (from the
/// server) is used when it has exactly that shape for this session id;
/// otherwise the name is built from the rollout's own UTC timestamp.
pub fn codex_target(
    root: &Path,
    session_id: &str,
    raw: &[u8],
    file_name: Option<&str>,
) -> Result<PathBuf> {
    if !is_uuid(session_id) {
        bail!("refusing to write session '{session_id}': not a UUID");
    }
    let stamp = match file_name.and_then(|n| rollout_stamp(n, session_id)) {
        Some(s) => s,
        None => utc_stamp(raw)?,
    };
    let (y, mo, d) = (&stamp[0..4], &stamp[5..7], &stamp[8..10]);
    Ok(root
        .join(y)
        .join(mo)
        .join(d)
        .join(format!("rollout-{stamp}-{session_id}.jsonl")))
}

/// `YYYY-MM-DDTHH-MM-SS` from `rollout-<that>-<id>.jsonl`, digits checked.
fn rollout_stamp(name: &str, session_id: &str) -> Option<String> {
    let stamp = name
        .strip_prefix("rollout-")?
        .strip_suffix(&format!("-{session_id}.jsonl"))?;
    let ok = stamp.len() == 19
        && stamp.char_indices().all(|(i, c)| match i {
            4 | 7 | 13 | 16 => c == '-',
            10 => c == 'T',
            _ => c.is_ascii_digit(),
        });
    ok.then(|| stamp.to_string())
}

/// `YYYY-MM-DDTHH-MM-SS` from the first line's `session_meta` timestamp.
fn utc_stamp(raw: &[u8]) -> Result<String> {
    let first = raw.split(|b| *b == b'\n').next().unwrap_or_default();
    let meta: serde_json::Value = serde_json::from_slice(first).unwrap_or_default();
    let ts = meta["payload"]["timestamp"]
        .as_str()
        .or_else(|| meta["timestamp"].as_str())
        .ok_or_else(|| anyhow!("rollout has no session_meta timestamp"))?;
    // 2026-11-20T12:01:43.452Z -> 2026-11-20T12-01-43
    let stamp: String = ts
        .chars()
        .take(19)
        .map(|c| if c == ':' { '-' } else { c })
        .collect();
    rollout_stamp(&format!("rollout-{stamp}-x.jsonl"), "x")
        .ok_or_else(|| anyhow!("bad timestamp {ts:?}"))
}

/// Write a new file under `root`: the target must stay inside `root`, no
/// directory on the way may be a symlink, and an existing file is never
/// overwritten.
pub fn write_new(root: &Path, target: &Path, bytes: &[u8]) -> Result<()> {
    let rel = target
        .strip_prefix(root)
        .map_err(|_| anyhow!("refusing to write outside {}", root.display()))?;
    if rel
        .components()
        .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        bail!("refusing path {}", target.display());
    }
    std::fs::create_dir_all(root)?;
    let mut dir = root.to_path_buf();
    let parts: Vec<_> = rel.components().collect();
    for part in &parts[..parts.len().saturating_sub(1)] {
        dir.push(part);
        match std::fs::symlink_metadata(&dir) {
            Ok(m) if m.file_type().is_symlink() => {
                bail!("refusing to write through symlink {}", dir.display())
            }
            Ok(_) => {}
            Err(_) => std::fs::create_dir(&dir)?,
        }
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts
        .open(target)
        .with_context(|| format!("{} already exists or cannot be created", target.display()))?;
    if let Err(e) = f.write_all(bytes).and_then(|_| f.sync_all()) {
        // A truncated transcript would be mistaken for a complete one later.
        let _ = std::fs::remove_file(target);
        return Err(e.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn servers_need_https_except_localhost() {
        assert!(check_server("https://cloud.example.com").is_ok());
        assert!(check_server("http://localhost:8787").is_ok());
        assert!(check_server("http://127.0.0.1:8787/").is_ok());
        assert!(check_server("http://evil.example").is_err());
        assert!(check_server("http://localhost.evil.example").is_err());
        assert!(check_server("http://localhost:80@evil.example/").is_err());
        assert!(check_server("https://user:pw@cloud.example.com").is_err());
        assert!(check_server("ftp://cloud.example.com").is_err());
    }

    #[test]
    fn cwd_encoding_matches_claude_code() {
        assert_eq!(
            encode_cwd(Path::new("/Users/us/coding/wend")),
            "-Users-us-coding-wend"
        );
        assert_eq!(
            encode_cwd(Path::new("/Users/us/Library/Application Support/qoop/qass")),
            "-Users-us-Library-Application-Support-qoop-qass"
        );
        assert_eq!(encode_cwd(Path::new("/x/.claude/a_b")), "-x--claude-a-b");
    }

    #[test]
    fn rehydrate_paths_are_built_locally_and_safely() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("projects");
        let id = "7242d5ee-3e5c-43aa-89e6-ce3b2754602c";
        assert!(claude_target(&root, Path::new("/p"), "../../etc/passwd").is_err());
        assert!(claude_target(&root, Path::new("/p"), "not-a-uuid").is_err());
        let target = claude_target(&root, Path::new("/a/../b"), id).unwrap();
        assert!(target.starts_with(&root));
        write_new(&root, &target, b"{}\n").unwrap();
        assert!(
            write_new(&root, &target, b"x").is_err(),
            "must not overwrite"
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"{}\n");
        assert!(write_new(&root, &dir.path().join("elsewhere.jsonl"), b"x").is_err());

        #[cfg(unix)]
        {
            let outside = dir.path().join("outside");
            std::fs::create_dir(&outside).unwrap();
            std::os::unix::fs::symlink(&outside, root.join("-link")).unwrap();
            let via_link = root.join("-link").join(format!("{id}.jsonl"));
            assert!(write_new(&root, &via_link, b"x").is_err());
            assert!(!outside.join(format!("{id}.jsonl")).exists());
        }
    }

    #[test]
    fn codex_target_uses_the_rollout_timestamp() {
        let id = "01a1f8d5-7e06-7112-9765-041bd071fcb4";
        let raw = br#"{"timestamp":"2026-11-20T12:01:43.613Z","type":"session_meta","payload":{"id":"x","timestamp":"2026-11-20T12:01:43.452Z"}}"#;
        let t = codex_target(Path::new("/r"), id, raw, None).unwrap();
        assert_eq!(
            t,
            Path::new("/r/2026/11/20").join(format!("rollout-2026-11-20T12-01-43-{id}.jsonl"))
        );
        let bad = br#"{"payload":{"timestamp":"20../11-20T12:01:43Z"}}"#;
        assert!(codex_target(Path::new("/r"), id, bad, None).is_err());
        // The server's file name wins when it has the exact shape (local time).
        let named = format!("rollout-2026-11-20T15-01-43-{id}.jsonl");
        assert_eq!(
            codex_target(Path::new("/r"), id, raw, Some(&named)).unwrap(),
            Path::new("/r/2026/11/20").join(&named)
        );
        // Anything else is ignored.
        for evil in [
            format!("../../x-{id}.jsonl"),
            format!("rollout-2026-11-20T15-01-43-{id}.jsonl/../../x"),
            "rollout-2026-11-20T15-01-43-other.jsonl".to_string(),
        ] {
            assert_eq!(
                codex_target(Path::new("/r"), id, raw, Some(&evil)).unwrap(),
                t,
                "{evil}"
            );
        }
    }

    #[test]
    fn wend_server_alone_is_refused() {
        assert!(from_env(None, Some("https://evil.example".into())).is_err());
        assert_eq!(from_env(None, None).unwrap(), None);
        assert!(from_env(Some("t".into()), None).is_err());
        let (server, token) = from_env(Some("t".into()), Some("https://s".into()))
            .unwrap()
            .unwrap();
        assert_eq!((server.as_str(), token.as_str()), ("https://s", "t"));
    }

    #[test]
    fn dry_run_hits_never_carry_the_secret() {
        let dir = tempfile::tempdir().unwrap();
        let secret = "ghp_aB3dE5fG7hJ9kL2mN4pQ6rS8tU0vW1xY2zA4";
        let path = dir.path().join("s.jsonl");
        std::fs::write(
            &path,
            format!("{{\"type\":\"user\",\"message\":{{\"content\":\"token {secret}\"}}}}\n"),
        )
        .unwrap();
        let p = prepare(Candidate {
            path,
            source: "claude",
            mtime: 0,
            size: 0,
        })
        .unwrap();
        assert!(!p.hits.is_empty());
        for masks in p.hits.values() {
            for m in masks {
                assert!(
                    !secret.contains(m.trim_end_matches("...")) || m.len() <= 7,
                    "{m}"
                );
                assert!(!m.contains(&secret[4..]), "{m}");
            }
        }
        let body = zstd::decode_all(&p.compressed[..]).unwrap();
        assert!(!String::from_utf8(body).unwrap().contains(secret));
    }

    #[test]
    fn paths_are_header_safe() {
        assert_eq!(percent_encode("/a b/ç%.jsonl"), "/a%20b/%C3%A7%25.jsonl");
    }

    #[cfg(unix)]
    #[test]
    fn credentials_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("wend").join("credentials.json");
        write_private(&p, b"{}").unwrap();
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
