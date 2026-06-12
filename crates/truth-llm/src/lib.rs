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

/// Extract a claim. The deterministic regex extractor runs FIRST and wins
/// whenever it produces a checkable claim — it is reliable and reflects the
/// hand-tuned, corpus-gated grammar. The LLM is consulted ONLY as a fallback
/// for phrasings regex refuses (`unknown`/not checkable), adding recall on
/// natural language without letting a weak model override a confident parse.
///
/// This ordering matters: testing a 1.5B local model as the SOLE extractor
/// showed it re-typing "MAX_RETRIES is 3" as usage_count and turning a clean
/// regex catch into Inconclusive. Regex-first keeps every win regex already
/// has; the LLM only fills the gaps. The engine still decides every verdict —
/// neither extractor decides truth.
pub fn extract_claim(config: &Config, text: &str) -> StructuredClaim {
    let regex_claim = RegexExtractor.extract(text);
    if !config.llm.enabled || regex_claim.is_checkable {
        return regex_claim;
    }
    // Regex refused and the LLM is enabled — try it for the extra recall.
    let key = Config::resolve_env(&config.llm.api_key_env);
    let extractor = OpenAiCompatibleExtractor::new(
        &config.llm.base_url,
        &config.llm.model,
        key,
        config.llm.timeout_ms,
    );
    // `try_extract` returns None on any failure; fall back to the (refused)
    // regex claim so behaviour with a dead LLM endpoint is identical to
    // LLM-disabled.
    extractor.try_extract(text).unwrap_or(regex_claim)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(enabled: bool, base_url: &str) -> Config {
        let mut c = Config::from_toml_str("").unwrap();
        c.llm.enabled = enabled;
        c.llm.base_url = base_url.into();
        // Short timeout so the unreachable-endpoint path returns fast.
        c.llm.timeout_ms = 200;
        c
    }

    #[test]
    fn llm_disabled_uses_regex() {
        let c = cfg(false, "http://127.0.0.1:1/v1");
        let claim = extract_claim(&c, "MAX_RETRIES is 5");
        assert!(claim.is_checkable);
        assert_eq!(claim.expected_number(), Some(5.0));
    }

    #[test]
    fn checkable_regex_claim_short_circuits_llm() {
        // Even with the LLM "enabled" but pointed at a dead endpoint, a claim
        // regex can parse must NOT touch the LLM (no timeout, no degradation).
        // If it consulted the LLM it would block for the timeout and fall back;
        // either way the regex result must win unchanged.
        let c = cfg(true, "http://127.0.0.1:1/v1");
        let claim = extract_claim(&c, "MAX_RETRIES is 3");
        assert!(claim.is_checkable);
        assert_eq!(claim.expected_number(), Some(3.0));
    }

    #[test]
    fn refused_regex_with_dead_llm_stays_refused() {
        // Regex refuses; LLM endpoint is dead → must fall back to the refused
        // regex claim, never panic or invent one.
        let c = cfg(true, "http://127.0.0.1:1/v1");
        let claim = extract_claim(&c, "I think the architecture is elegant");
        assert!(!claim.is_checkable);
    }
}
