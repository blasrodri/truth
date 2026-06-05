//! Optional OpenAI-compatible claim extractor. Sends a strict JSON-schema
//! prompt to the configured endpoint and parses the structured claim. On ANY
//! error (network, timeout, bad JSON) it returns `None` so the caller can fall
//! back to the deterministic `RegexExtractor`.

use crate::extract::{ClaimExtractor, RegexExtractor};
use std::time::Duration;
use truth_core::claim::StructuredClaim;

pub struct OpenAiCompatibleExtractor {
    base_url: String,
    model: String,
    api_key: Option<String>,
    timeout: Duration,
}

const SYSTEM_PROMPT: &str = r#"You extract a single checkable engineering claim from a message.
Respond ONLY with a JSON object of this exact shape, no prose:
{"is_checkable":bool,"claim_type":string,"subject":string|null,"predicate":string|null,
"operator":"equals|not_equals|greater_than|less_than|exists|not_exists|unknown",
"value":number|string|bool|null,"unit":string|null,"time_window":string|null,
"environment":string|null,"confidence":number,"needs_clarification":bool,
"clarification_question":string|null}
Allowed claim_type: usage_count, error_still_happening, latest_occurrence, route_exists,
config_value, env_var_exists, dependency_used, retry_count, timeout_value, version_required,
job_last_success, feature_flag_enabled, unknown."#;

impl OpenAiCompatibleExtractor {
    pub fn new(
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key: Option<String>,
        timeout_ms: u64,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            model: model.into(),
            api_key,
            timeout: Duration::from_millis(timeout_ms),
        }
    }

    /// Try the LLM; `None` on any failure.
    pub fn try_extract(&self, text: &str) -> Option<StructuredClaim> {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let body = serde_json::json!({
            "model": self.model,
            "temperature": 0,
            "response_format": {"type": "json_object"},
            "messages": [
                {"role": "system", "content": SYSTEM_PROMPT},
                {"role": "user", "content": text}
            ]
        });

        let agent = ureq::AgentBuilder::new()
            .timeout(self.timeout)
            .build();
        let mut req = agent.post(&url).set("content-type", "application/json");
        if let Some(key) = &self.api_key {
            req = req.set("authorization", &format!("Bearer {key}"));
        }
        let resp = req.send_json(body).ok()?;
        let v: serde_json::Value = resp.into_json().ok()?;
        let content = v["choices"][0]["message"]["content"].as_str()?;
        let claim: StructuredClaim = serde_json::from_str(content).ok()?;
        Some(claim)
    }
}

impl ClaimExtractor for OpenAiCompatibleExtractor {
    /// Extract via LLM, falling back to the deterministic extractor.
    fn extract(&self, text: &str) -> StructuredClaim {
        self.try_extract(text)
            .unwrap_or_else(|| RegexExtractor.extract(text))
    }
}
