//! Local semantic search (opt-in `semantic` feature) — CHUNK level via fastembed.
//!
//! Each session is split into message-aligned text chunks (the user's own prompts;
//! assistant/tool/log turns excluded — what *you asked* defines the topic and
//! keeps the corpus small/fast). Every chunk is embedded with `fastembed`
//! (ONNX Runtime — fast CPU throughput) using the multilingual
//! `multilingual-e5-small` model (384-d, good Turkish). Search embeds the query,
//! scores chunks by cosine (= dot; fastembed L2-normalizes), rolls chunks up to
//! their session (max chunk score), and fuses with keyword via RRF.
//!
//! e5 is asymmetric: documents get a `passage:` prefix, queries a `query:` prefix.

use crate::error::{Error, Result};
use crate::store::{ChunkVec, SearchHit, Store};
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};

const MODEL_NAME: &str = "multilingual-e5-small";
/// Target chunk size in bytes — conservative for multi-byte (Turkish) text so we
/// stay under the model's 512-token limit (fastembed truncates as a safety net).
const CHUNK_BYTES: usize = 1200;

fn err<E: std::fmt::Display>(ctx: &str) -> impl Fn(E) -> Error + '_ {
    move |e| Error::InvalidData(format!("{ctx}: {e}"))
}

/// How many CPU threads embedding may use. Gentle by default (~a quarter of the
/// cores) so a full embed doesn't pin the whole machine; override with
/// `WEND_EMBED_THREADS`. ONNX Runtime would otherwise grab every core.
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

/// A loaded embedding model.
pub struct Embedder {
    model: TextEmbedding,
}

impl Embedder {
    /// Load the model, downloading + caching it on first use.
    pub fn load() -> Result<Self> {
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

    /// Embed passages (documents). fastembed L2-normalizes the output.
    pub fn embed_passages(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let docs: Vec<String> = texts.iter().map(|t| format!("passage: {t}")).collect();
        self.model.embed(docs, None).map_err(err("embed"))
    }

    /// Embed a single query.
    pub fn embed_query(&mut self, query: &str) -> Result<Vec<f32>> {
        self.model
            .embed(vec![format!("query: {query}")], None)
            .map_err(err("embed query"))?
            .pop()
            .ok_or_else(|| Error::InvalidData("empty query embedding".into()))
    }
}

/// Split ordered message texts into message-aligned, ~`CHUNK_BYTES` chunks.
/// An oversized single message is hard-split (on char boundaries).
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
            let chars: Vec<char> = m.chars().collect();
            let mut i = 0;
            while i < chars.len() {
                let end = (i + CHUNK_BYTES).min(chars.len());
                chunks.push(chars[i..end].iter().collect());
                i = end;
            }
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

/// Chunk any un-chunked sessions, then embed any chunks without a vector.
/// Returns `(chunks_created, chunks_embedded)`. Resume-safe.
pub fn build_index(store: &mut Store) -> Result<(usize, usize)> {
    let mut created = 0;
    for pk in store.sessions_without_chunks()? {
        let msgs = store.semantic_messages(pk)?;
        for (i, text) in chunk_texts(&msgs).into_iter().enumerate() {
            store.insert_chunk(pk, i as i64, &text)?;
            created += 1;
        }
    }

    let pending = store.chunks_needing_vectors()?;
    if pending.is_empty() {
        return Ok((created, 0));
    }
    let mut embedder = Embedder::load()?;
    let mut embedded = 0;
    for batch in pending.chunks(256) {
        let texts: Vec<String> = batch.iter().map(|(_, t)| t.clone()).collect();
        let vectors = embedder.embed_passages(&texts)?;
        for ((cid, _), v) in batch.iter().zip(vectors.iter()) {
            store.store_chunk_vector(*cid, MODEL_NAME, v)?;
            embedded += 1;
        }
    }
    Ok((created, embedded))
}

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

    // Semantic: score every chunk, keep the best chunk per session.
    let mut semantic: Vec<SearchHit> = Vec::new();
    let chunks = store.all_chunk_vectors()?;
    if !chunks.is_empty() {
        let mut embedder = Embedder::load()?;
        let qv = embedder.embed_query(query)?;
        let mut best: HashMap<String, (f32, ChunkVec)> = HashMap::new();
        for c in chunks {
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
}
