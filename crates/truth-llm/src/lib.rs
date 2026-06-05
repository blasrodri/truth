//! `truth-llm` — claim extraction, query planning, and response generation.
//!
//! The deterministic regex extractor is the default and always available; the
//! OpenAI-compatible client is optional and degrades to the regex extractor.

pub mod client;
#[cfg(feature = "embeddings")]
pub mod embed;
pub mod extract;
pub mod plan;
pub mod respond;

pub use client::OpenAiCompatibleExtractor;
#[cfg(feature = "embeddings")]
pub use embed::EmbeddingResolver;
pub use extract::{ClaimExtractor, RegexExtractor};
pub use plan::plan_for;
pub use respond::{render, ResponseInput};

use truth_core::claim::StructuredClaim;
use truth_core::config::Config;

/// Extract a claim using the LLM if enabled/configured, else deterministic.
pub fn extract_claim(config: &Config, text: &str) -> StructuredClaim {
    if config.llm.enabled {
        let key = Config::resolve_env(&config.llm.api_key_env);
        let extractor = OpenAiCompatibleExtractor::new(
            &config.llm.base_url,
            &config.llm.model,
            key,
            config.llm.timeout_ms,
        );
        extractor.extract(text)
    } else {
        RegexExtractor.extract(text)
    }
}
