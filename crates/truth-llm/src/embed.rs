//! Static-embedding concept resolver (feature `embeddings`).
//!
//! Uses a model2vec static model (a distilled token→vector lookup table — no
//! neural-net inference, pure Rust, deterministic) to resolve claim subjects to
//! indexed concepts by cosine similarity. This catches pure-vocabulary-mismatch
//! cases the deterministic `FuzzyResolver` misses ("shopping cart" → "/checkout"),
//! while keeping the default build dependency-free.
//!
//! The model is loaded from a local directory (no network); the path is provided
//! by configuration. If loading fails, callers fall back to the fuzzy resolver.

use anyhow::{Context, Result};
use model2vec_rs::model::StaticModel;
use truth_core::concept::{Candidate, ConceptResolver, Resolution};

pub struct EmbeddingResolver {
    model: StaticModel,
    /// Minimum cosine similarity to accept a match.
    threshold: f32,
}

impl EmbeddingResolver {
    /// Load a static model from a local directory (containing the safetensors +
    /// tokenizer). No network access.
    pub fn from_path(dir: &str, threshold: f32) -> Result<Self> {
        let model = StaticModel::from_pretrained(dir, None, None, None)
            .with_context(|| format!("loading embedding model from {dir}"))?;
        Ok(EmbeddingResolver { model, threshold })
    }

    fn embed(&self, texts: &[String]) -> Vec<Vec<f32>> {
        self.model.encode(texts)
    }
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na * nb)
    }
}

impl ConceptResolver for EmbeddingResolver {
    fn resolve(&self, subject: &str, candidates: &[Candidate]) -> Option<Resolution> {
        if candidates.is_empty() {
            return None;
        }
        let mut texts: Vec<String> = Vec::with_capacity(candidates.len() + 1);
        texts.push(subject.to_string());
        // Embed the human search text (enriched label), not the raw identifier.
        texts.extend(candidates.iter().map(|c| c.search_text.clone()));

        let vecs = self.embed(&texts);
        let (subject_vec, cand_vecs) = vecs.split_first()?;

        let mut best: Option<(f32, &str)> = None;
        for (v, c) in cand_vecs.iter().zip(candidates) {
            let s = cosine(subject_vec, v);
            if best.map(|(bs, _)| s > bs).unwrap_or(true) {
                best = Some((s, &c.label));
            }
        }
        match best {
            Some((score, label)) if score >= self.threshold => Some(Resolution {
                label: label.to_string(),
                confidence: score.clamp(0.0, 1.0),
            }),
            _ => None,
        }
    }
}
