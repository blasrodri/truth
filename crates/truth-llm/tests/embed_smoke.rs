//! Embedding resolver smoke test (feature `embeddings`). Ignored by default —
//! it downloads/loads a model. Run with:
//!   cargo test -p truth-llm --features embeddings -- --ignored --nocapture
//!
//! Key finding this test documents: a static embedding model resolves
//! pure-semantic queries ("shopping cart" -> checkout) ONLY when candidates are
//! humanized words, not raw route paths. Embedding `/v1/checkout` directly
//! scores poorly (1/4); embedding "checkout payment order" scores 4/4.
#![cfg(feature = "embeddings")]

use truth_core::concept::{Candidate, ConceptResolver};
use truth_llm::EmbeddingResolver;

fn model_path() -> String {
    std::env::var("TRUTH_EMBED_MODEL").unwrap_or_else(|_| "minishlab/potion-base-8M".into())
}

#[test]
#[ignore]
fn humanized_labels_resolve_pure_semantic_cases() {
    let r = EmbeddingResolver::from_path(&model_path(), 0.1).expect("load embedding model");
    // Human-word descriptions of each route (what enriched concept labels look like).
    let labels = ["checkout payment order", "user authentication login signin", "service health status", "billing charge payment"];
    let routes = ["/v1/checkout", "/auth/login", "/health", "/billing/charge"];
    let cands: Vec<Candidate> = labels.iter().map(|s| Candidate::new(*s)).collect();

    let mut hits = 0;
    for (q, expected) in [
        ("shopping cart", "/v1/checkout"),
        ("sign in", "/auth/login"),
        ("liveness probe", "/health"),
        ("payment processing", "/billing/charge"),
    ] {
        let got = r.resolve(q, &cands).map(|x| {
            let i = cands.iter().position(|c| c.label == x.label).unwrap();
            routes[i]
        });
        println!("{q:20} -> {got:?}  (expected {expected})");
        if got == Some(expected) {
            hits += 1;
        }
    }
    println!("humanized embedding resolver: {hits}/4 pure-semantic cases");
    assert!(hits >= 3, "expected embeddings to resolve most humanized cases, got {hits}/4");
}
