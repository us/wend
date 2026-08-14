//! Semantic search over CHUNK-level embeddings, with two interchangeable
//! backends (both opt-in features; the default build has neither and stays
//! offline).
//!
//! Each session is split into message-aligned text chunks (the user's own
//! prompts; assistant/tool/log turns excluded — what *you asked* defines the
//! topic and keeps the corpus small/fast). Search embeds the query, scores
//! chunks by cosine (= dot; both backends emit L2-normalized vectors), rolls
//! chunks up to their session (max chunk score), and fuses with keyword via RRF.
//!
//! Backends:
//! - `semantic`: local `fastembed` / ONNX, `multilingual-e5-small` (384-d).
//!   e5 is asymmetric, so documents get a `passage:` prefix and queries a
//!   `query:` prefix. Offline, free, and measurably weaker.
//! - `azure`: Azure OpenAI `text-embedding-3-large` at 1024-d. Symmetric — it
//!   must NOT get the e5 prefixes. Sends text off-machine; see [`azure`].
//!
//! Measured on the author's real corpus (500 prompts, 34 Turkish queries):
//! Azure MRR@10 0.771, local e5-small 0.381, keyword-only 0.000.

#[cfg(feature = "azure")]
pub mod azure;
pub mod cases;

use crate::error::{Error, Result};
use crate::store::{ChunkVec, SearchHit, Store};

/// Target chunk size in bytes.
const CHUNK_BYTES: usize = 1200;

/// How many chunks go into one embedding request.
const BATCH: usize = 96;

#[cfg(feature = "semantic")]
fn err<E: std::fmt::Display>(ctx: &str) -> impl Fn(E) -> Error + '_ {
    move |e| Error::InvalidData(format!("{ctx}: {e}"))
}

/// Which backend this process will use, resolved from the environment *without*
/// loading anything expensive.
///
/// Split from [`Embedder`] on purpose: the model id is needed to ask the store
/// what still needs embedding, and answering "nothing" should not have cost a
/// multi-hundred-MB model download first.
enum Selection {
    #[cfg(feature = "semantic")]
    Local,
    #[cfg(feature = "azure")]
    Azure(azure::Config),
}

/// Resolve the backend from the environment.
///
/// Azure wins when configured. Partial Azure configuration is a hard error and
/// never a quiet fallback to local: falling back would treat every Azure vector
/// as stale and overwrite the whole corpus with local vectors on the next
/// `--embed`, silently trading 0.771 for 0.381 (and the cost of re-embedding)
/// because of one typo'd variable.
fn select() -> Result<Selection> {
    #[cfg(feature = "azure")]
    {
        if let Some(cfg) = azure::Config::from_env()? {
            return Ok(Selection::Azure(cfg));
        }
    }
    #[cfg(feature = "semantic")]
    {
        Ok(Selection::Local)
    }
    #[cfg(not(feature = "semantic"))]
    {
        Err(Error::InvalidData(
            "no embedding backend configured: this build has only the Azure backend, \
             so set WEND_AZURE_ENDPOINT, WEND_AZURE_KEY and WEND_AZURE_DEPLOYMENT"
                .into(),
        ))
    }
}

impl Selection {
    fn model_id(&self) -> String {
        match self {
            #[cfg(feature = "semantic")]
            Selection::Local => LOCAL_MODEL.to_string(),
            #[cfg(feature = "azure")]
            Selection::Azure(cfg) => cfg.model_id(),
        }
    }
}

/// The model id the current environment will embed with. Used as the storage key
/// so vectors from different models never get compared to each other.
pub fn current_model_id() -> Result<String> {
    Ok(select()?.model_id())
}

/// Which backend is active. A type rather than a string so callers branch on a
/// variant instead of prefix-matching a display name that anyone could reword.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Local,
    Azure,
}

impl std::fmt::Display for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Backend::Local => "local (fastembed)",
            Backend::Azure => "azure",
        })
    }
}

/// The backend this environment resolves to, for `wend doctor` and messaging.
pub fn active_backend() -> Result<Backend> {
    Ok(match select()? {
        #[cfg(feature = "semantic")]
        Selection::Local => Backend::Local,
        #[cfg(feature = "azure")]
        Selection::Azure(_) => Backend::Azure,
    })
}

/// A loaded embedding backend.
///
/// Every method is a `match` whose arms mirror the variant `cfg`s exactly. Miss
/// one and the build breaks only in a single-feature configuration — which is
/// why the test matrix compiles all four combinations.
pub enum Embedder {
    /// Boxed: the loaded ONNX session is ~1.2 KB inline, which would make every
    /// `Embedder` that size even on the Azure path.
    #[cfg(feature = "semantic")]
    Local(Box<Local>),
    #[cfg(feature = "azure")]
    Azure(azure::Azure),
}

impl Embedder {
    /// Load the selected backend, downloading/caching the local model on first use.
    pub fn load() -> Result<Self> {
        match select()? {
            #[cfg(feature = "semantic")]
            Selection::Local => Ok(Self::Local(Box::new(Local::load()?))),
            #[cfg(feature = "azure")]
            Selection::Azure(cfg) => Ok(Self::Azure(azure::Azure::new(cfg)?)),
        }
    }

    /// Embed documents. Both backends return L2-normalized vectors.
    pub fn embed_passages(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        match self {
            #[cfg(feature = "semantic")]
            Self::Local(m) => m.embed_passages(texts),
            #[cfg(feature = "azure")]
            Self::Azure(a) => a.embed_passages(texts),
        }
    }

    /// Embed a single query.
    pub fn embed_query(&mut self, query: &str) -> Result<Vec<f32>> {
        match self {
            #[cfg(feature = "semantic")]
            Self::Local(m) => m.embed_query(query),
            #[cfg(feature = "azure")]
            Self::Azure(a) => a.embed_query(query),
        }
    }
}

// ---------------------------------------------------------------- local backend

#[cfg(feature = "semantic")]
const LOCAL_MODEL: &str = "multilingual-e5-small";

/// How many CPU threads local embedding may use. Gentle by default (~a quarter
/// of the cores) so a full embed doesn't pin the whole machine; override with
/// `WEND_EMBED_THREADS`. ONNX Runtime would otherwise grab every core.
#[cfg(feature = "semantic")]
pub fn embed_threads() -> usize {
    if let Ok(v) = std::env::var("WEND_EMBED_THREADS") {
        if let Ok(n) = v.parse::<usize>() {
            return n.max(1);
        }
    }
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    (cores / 4).max(1)
}

/// Local fastembed backend.
#[cfg(feature = "semantic")]
pub struct Local {
    model: fastembed::TextEmbedding,
}

#[cfg(feature = "semantic")]
impl Local {
    fn load() -> Result<Self> {
        use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
        let cache = crate::config::model_cache_dir()?;
        std::fs::create_dir_all(&cache)?;
        let model = TextEmbedding::try_new(
            InitOptions::new(EmbeddingModel::MultilingualE5Small)
                .with_cache_dir(cache)
                .with_intra_threads(embed_threads()) // gentle: don't pin every core
                .with_show_download_progress(true),
        )
        .map_err(err("load model"))?;
        Ok(Self { model })
    }

    fn embed_passages(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let docs: Vec<String> = texts.iter().map(|t| format!("passage: {t}")).collect();
        self.model.embed(docs, None).map_err(err("embed"))
    }

    fn embed_query(&mut self, query: &str) -> Result<Vec<f32>> {
        self.model
            .embed(vec![format!("query: {query}")], None)
            .map_err(err("embed query"))?
            .pop()
            .ok_or_else(|| Error::InvalidData("empty query embedding".into()))
    }
}

// -------------------------------------------------------------------- chunking

/// Split one oversized message into ≤`CHUNK_BYTES` pieces on char boundaries.
///
/// Measures bytes, not chars. The previous version tested `m.len()` (bytes) but
/// then cut on `CHUNK_BYTES` *chars*, so multi-byte text overshot badly: on the
/// author's real corpus 64% of chunks exceeded the 1200-byte target, topping out
/// at 2431 B (~767 tokens) — well past the local model's 512-token window, where
/// fastembed silently truncates.
fn hard_split(m: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for ch in m.chars() {
        if !cur.is_empty() && cur.len() + ch.len_utf8() > CHUNK_BYTES {
            out.push(std::mem::take(&mut cur));
        }
        cur.push(ch);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Split ordered message texts into message-aligned, ~`CHUNK_BYTES` chunks.
fn chunk_texts(msgs: &[String]) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut cur = String::new();
    for m in msgs {
        let m = m.trim();
        if m.is_empty() {
            continue;
        }
        if m.len() > CHUNK_BYTES {
            if !cur.is_empty() {
                chunks.push(std::mem::take(&mut cur));
            }
            chunks.extend(hard_split(m));
            continue;
        }
        if !cur.is_empty() && cur.len() + 1 + m.len() > CHUNK_BYTES {
            chunks.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push('\n');
        }
        cur.push_str(m);
    }
    if !cur.is_empty() {
        chunks.push(cur);
    }
    chunks
}

// ------------------------------------------------------------------- indexing

/// Chunk any un-chunked sessions. Returns how many chunks were created.
///
/// Split from [`embed_pending`] so a caller can chunk first, ask how much work
/// that produced, and tell the user what it will cost *before* spending money —
/// counting pending vectors before chunking always reports zero on a fresh index.
pub fn build_chunks(store: &mut Store) -> Result<usize> {
    let mut created = 0;
    for pk in store.sessions_without_chunks("prose")? {
        let msgs = store.semantic_messages(pk)?;
        let chunks = chunk_texts(&msgs);
        if !chunks.is_empty() {
            created += store.insert_session_chunks(pk, &chunks)?;
        }
    }
    Ok(created)
}

/// Embed every chunk lacking a vector for the current model. Resume-safe.
pub fn embed_pending(store: &mut Store) -> Result<usize> {
    let model = current_model_id()?;
    // `None`: every kind. Prose chunks and cases share the pipeline, and
    // filtering to one here would leave the other permanently unembedded.
    let pending = store.chunks_needing_vectors(&model, None)?;
    if pending.is_empty() {
        return Ok(0);
    }

    let mut embedder = Embedder::load()?;
    let mut embedded = 0;
    for batch in pending.chunks(BATCH) {
        let texts: Vec<String> = batch.iter().map(|(_, t)| t.clone()).collect();
        let vectors = embedder.embed_passages(&texts)?;
        if vectors.len() != batch.len() {
            return Err(Error::InvalidData(format!(
                "backend returned {} vectors for {} inputs",
                vectors.len(),
                batch.len()
            )));
        }
        let rows: Vec<(i64, Vec<f32>)> = batch
            .iter()
            .map(|(cid, _)| *cid)
            .zip(vectors)
            .collect::<Vec<_>>();
        embedded += store.store_chunk_vectors_batch(&model, &rows)?;
    }
    Ok(embedded)
}

// --------------------------------------------------------------------- search

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn snippet_of(text: &str) -> String {
    let one_line: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= 140 {
        one_line
    } else {
        let cut: String = one_line.chars().take(140).collect();
        format!("…{cut}…")
    }
}

/// Hybrid search: keyword (BM25) fused with chunk-level semantic (cosine) via RRF.
pub fn hybrid_search(store: &Store, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
    use std::collections::HashMap;

    let over = limit.saturating_mul(3).max(limit);
    let keyword = crate::search::search(store, query, over, None)?;

    // Semantic: score every chunk embedded by the *current* model, keep the best
    // chunk per session.
    let model = current_model_id()?;
    let mut semantic: Vec<SearchHit> = Vec::new();
    // Prose only: cases hold situation text, which would pollute ordinary search.
    let chunks = store.all_chunk_vectors(&model, "prose")?;
    if !chunks.is_empty() {
        let mut embedder = Embedder::load()?;
        let qv = embedder.embed_query(query)?;
        let mut best: HashMap<String, (f32, ChunkVec)> = HashMap::new();
        for c in chunks {
            if c.vec.len() != qv.len() {
                return Err(Error::InvalidData(format!(
                    "stored vector is {}-d but the query is {}-d — run `wend index --embed`",
                    c.vec.len(),
                    qv.len()
                )));
            }
            let s = dot(&qv, &c.vec);
            best.entry(c.session_id.clone())
                .and_modify(|e| {
                    if s > e.0 {
                        *e = (s, c.clone());
                    }
                })
                .or_insert((s, c));
        }
        let mut ranked: Vec<(f32, ChunkVec)> = best.into_values().collect();
        ranked.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        ranked.truncate(over);
        semantic = ranked
            .into_iter()
            .map(|(_, c)| SearchHit {
                session_id: c.session_id,
                title: c.title,
                project: c.project,
                line_no: 0,
                snippet: snippet_of(&c.text),
                rank: 0.0,
            })
            .collect();
    }

    // Reciprocal Rank Fusion (keyword first, so its richer snippet wins on ties).
    const K: f64 = 60.0;
    let mut score: HashMap<String, f64> = HashMap::new();
    let mut info: HashMap<String, SearchHit> = HashMap::new();
    for (rank, h) in keyword.iter().enumerate() {
        *score.entry(h.session_id.clone()).or_default() += 1.0 / (K + rank as f64 + 1.0);
        info.entry(h.session_id.clone())
            .or_insert_with(|| h.clone());
    }
    for (rank, h) in semantic.iter().enumerate() {
        *score.entry(h.session_id.clone()).or_default() += 1.0 / (K + rank as f64 + 1.0);
        info.entry(h.session_id.clone())
            .or_insert_with(|| h.clone());
    }

    let mut merged: Vec<(f64, SearchHit)> = score
        .into_iter()
        .filter_map(|(sid, sc)| info.remove(&sid).map(|h| (sc, h)))
        .collect();
    merged.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    Ok(merged.into_iter().take(limit).map(|(_, h)| h).collect())
}

#[cfg(test)]
mod tests {
    use super::{chunk_texts, dot, CHUNK_BYTES};

    #[test]
    fn dot_is_cosine_for_normalized_vectors() {
        let a = [0.6_f32, 0.8];
        assert!((dot(&a, &a) - 1.0).abs() < 1e-6);
        let b = [0.8_f32, -0.6];
        assert!(dot(&a, &b).abs() < 1e-6);
    }

    #[test]
    fn chunking_packs_and_splits() {
        assert_eq!(
            chunk_texts(&["hello".to_string(), "world".to_string()]).len(),
            1
        );
        assert!(chunk_texts(&["x".repeat(CHUNK_BYTES * 2 + 10)]).len() >= 3);
        assert!(chunk_texts(&["".to_string(), "  ".to_string()]).is_empty());
    }

    /// Regression: chunks are capped in BYTES, not chars. Turkish is ~1.6 bytes
    /// per char, so the old char-based split produced chunks up to 2431 B and
    /// blew past the local model's 512-token window.
    #[test]
    fn chunks_never_exceed_the_byte_cap() {
        let turkish = "çğıöşü ÇĞİÖŞÜ birazcık daha uzun bir cümle olsun diye ".repeat(200);
        assert!(turkish.len() > CHUNK_BYTES * 4, "test input must be big");
        for c in chunk_texts(&[turkish]) {
            assert!(
                c.len() <= CHUNK_BYTES,
                "chunk was {} bytes, cap is {CHUNK_BYTES}",
                c.len()
            );
        }
    }

    /// A multi-byte char must never be split across two chunks.
    #[test]
    fn hard_split_keeps_chars_intact() {
        let s = "ş".repeat(CHUNK_BYTES);
        for c in super::hard_split(&s) {
            assert!(c.chars().all(|ch| ch == 'ş'));
            assert!(c.len() <= CHUNK_BYTES);
        }
    }
}
