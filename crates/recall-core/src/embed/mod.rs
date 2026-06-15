//! Local semantic search (opt-in `semantic` feature).
//!
//! Embeds a representative text per session (title + first message) with a
//! pure-Rust Candle BERT model (`bge-small-en-v1.5`, CLS pooling, L2-normalized,
//! 384-d) and stores one vector per session. Search embeds the query and ranks
//! by cosine (= dot product, since vectors are normalized) over ~one vector per
//! session — brute force is plenty at this scale, so no ANN/extension is needed.
//! Keyword + semantic are fused with Reciprocal Rank Fusion.

use crate::error::{Error, Result};
use crate::store::{SearchHit, Store};

use candle_core::{DType, Device, IndexOp, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config};
use hf_hub::api::sync::ApiBuilder;
use hf_hub::{Repo, RepoType};
use tokenizers::{PaddingParams, PaddingStrategy, Tokenizer, TruncationParams};

const MODEL_ID: &str = "BAAI/bge-small-en-v1.5";
const MAX_TOKENS: usize = 512;

fn err<E: std::fmt::Display>(ctx: &str) -> impl Fn(E) -> Error + '_ {
    move |e| Error::InvalidData(format!("{ctx}: {e}"))
}

/// A loaded embedding model.
pub struct Embedder {
    model: BertModel,
    tokenizer: Tokenizer,
    device: Device,
}

impl Embedder {
    /// Load the model, downloading + caching it on first use.
    pub fn load() -> Result<Self> {
        let device = Device::Cpu;
        let cache = crate::config::model_cache_dir()?;
        std::fs::create_dir_all(&cache)?;
        let api = ApiBuilder::new()
            .with_cache_dir(cache)
            .build()
            .map_err(err("hf-hub init"))?;
        let repo = api.repo(Repo::new(MODEL_ID.to_string(), RepoType::Model));

        let config_path = repo.get("config.json").map_err(err("download config"))?;
        let tok_path = repo
            .get("tokenizer.json")
            .map_err(err("download tokenizer"))?;
        let weights = repo
            .get("model.safetensors")
            .map_err(err("download weights"))?;

        let cfg: Config = serde_json::from_str(&std::fs::read_to_string(config_path)?)
            .map_err(err("parse config"))?;
        let mut tokenizer = Tokenizer::from_file(tok_path).map_err(err("load tokenizer"))?;
        tokenizer
            .with_truncation(Some(TruncationParams {
                max_length: MAX_TOKENS,
                ..Default::default()
            }))
            .map_err(err("truncation"))?;
        tokenizer.with_padding(Some(PaddingParams {
            strategy: PaddingStrategy::BatchLongest,
            ..Default::default()
        }));

        let vb = unsafe {
            VarBuilder::from_mmaped_safetensors(&[weights], DType::F32, &device)
                .map_err(err("load safetensors"))?
        };
        let model = BertModel::load(vb, &cfg).map_err(err("build model"))?;
        Ok(Self {
            model,
            tokenizer,
            device,
        })
    }

    /// Embed a batch of texts → L2-normalized 384-d vectors (CLS pooling).
    pub fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let encodings = self
            .tokenizer
            .encode_batch(texts.to_vec(), true)
            .map_err(err("tokenize"))?;

        let mut ids = Vec::with_capacity(encodings.len());
        let mut masks = Vec::with_capacity(encodings.len());
        for enc in &encodings {
            ids.push(Tensor::new(enc.get_ids(), &self.device).map_err(err("ids tensor"))?);
            masks.push(
                Tensor::new(enc.get_attention_mask(), &self.device).map_err(err("mask tensor"))?,
            );
        }
        let token_ids = Tensor::stack(&ids, 0).map_err(err("stack ids"))?;
        let attn = Tensor::stack(&masks, 0).map_err(err("stack masks"))?;
        let token_type_ids = token_ids.zeros_like().map_err(err("ttype"))?;

        let out = self
            .model
            .forward(&token_ids, &token_type_ids, Some(&attn))
            .map_err(err("forward"))?;

        // CLS pooling: hidden state of token 0 → (batch, hidden).
        let cls = out.i((.., 0)).map_err(err("cls"))?;
        // L2 normalize.
        let norm = cls
            .sqr()
            .map_err(err("sqr"))?
            .sum_keepdim(1)
            .map_err(err("sum"))?
            .sqrt()
            .map_err(err("sqrt"))?;
        let normalized = cls.broadcast_div(&norm).map_err(err("normalize"))?;
        normalized.to_vec2::<f32>().map_err(err("to_vec2"))
    }
}

/// Embed all sessions that don't have a vector yet. Returns the count embedded.
pub fn build_index(store: &mut Store) -> Result<usize> {
    let pending = store.sessions_needing_vectors()?;
    if pending.is_empty() {
        return Ok(0);
    }
    let embedder = Embedder::load()?;
    let mut done = 0;
    // Embed in modest batches to bound memory.
    for chunk in pending.chunks(32) {
        let texts: Vec<String> = chunk
            .iter()
            .map(|(_, t)| {
                if t.is_empty() {
                    " ".to_string()
                } else {
                    t.clone()
                }
            })
            .collect();
        let vectors = embedder.embed_batch(&texts)?;
        for ((pk, _), v) in chunk.iter().zip(vectors.iter()) {
            store.store_session_vector(*pk, MODEL_ID, v)?;
            done += 1;
        }
    }
    Ok(done)
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Hybrid search: keyword (BM25) fused with semantic (cosine) via RRF.
pub fn hybrid_search(store: &Store, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
    use std::collections::HashMap;

    let over = limit.saturating_mul(3).max(limit);
    let keyword = crate::search::search(store, query, over)?;

    // Semantic ranking over session vectors.
    let mut semantic: Vec<SearchHit> = Vec::new();
    let vectors = store.all_session_vectors()?;
    if !vectors.is_empty() {
        let embedder = Embedder::load()?;
        let qv = embedder
            .embed_batch(&[query.to_string()])?
            .pop()
            .ok_or_else(|| Error::InvalidData("empty query embedding".into()))?;
        let mut scored: Vec<(f32, &crate::store::VecRow)> =
            vectors.iter().map(|r| (dot(&qv, &r.vec), r)).collect();
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        for (_s, r) in scored.into_iter().take(over) {
            semantic.push(SearchHit {
                session_id: r.session_id.clone(),
                title: r.title.clone(),
                project: r.project.clone(),
                line_no: 0,
                snippet: "(semantic match)".to_string(),
                rank: 0.0,
            });
        }
    }

    // Reciprocal Rank Fusion.
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
        // keep keyword's richer snippet if we already have it
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
    use super::dot;

    #[test]
    fn dot_is_cosine_for_normalized_vectors() {
        // identical normalized vectors → 1.0; orthogonal → 0.0
        let a = [0.6_f32, 0.8];
        assert!((dot(&a, &a) - 1.0).abs() < 1e-6);
        let b = [0.8_f32, -0.6];
        assert!(dot(&a, &b).abs() < 1e-6);
    }
}
