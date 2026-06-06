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

const SYSTEM_PROMPT: &str = r#"You extract a single checkable engineering claim from a message about a codebase.
Respond ONLY with a JSON object of this exact shape, no prose, no markdown fences:
{"is_checkable":bool,"claim_type":string,"subject":string|null,"predicate":string|null,
"operator":"equals|not_equals|greater_than|less_than|exists|not_exists|unknown",
"value":number|string|bool|null,"unit":string|null,"time_window":string|null,
"environment":string|null,"confidence":number,"needs_clarification":bool,
"clarification_question":string|null}

Allowed claim_type: usage_count, error_still_happening, latest_occurrence, route_exists,
config_value, env_var_exists, dependency_used, retry_count, timeout_value, version_required,
job_last_success, feature_flag_enabled, unknown.

The "subject" is the specific symbol/route/constant/dependency the claim is about
(e.g. a constant name like MAX_RETRIES, a function/type like JoinHandle, a
dependency like serde, a route like /v1/checkout). Use usage_count for "X is
unused" / "nobody uses X" / "is X still used" (operator equals, value 0).

Examples:
"PARKED is 1" -> {"is_checkable":true,"claim_type":"config_value","subject":"PARKED","operator":"equals","value":1,"confidence":0.9}
"JoinHandle is unused" -> {"is_checkable":true,"claim_type":"usage_count","subject":"JoinHandle","operator":"equals","value":0,"confidence":0.85}
"nobody uses spawn_blocking anymore" -> {"is_checkable":true,"claim_type":"usage_count","subject":"spawn_blocking","operator":"equals","value":0,"confidence":0.85}
"the bytes crate is unused" -> {"is_checkable":true,"claim_type":"dependency_used","subject":"bytes","operator":"equals","value":0,"confidence":0.8}
"the architecture is elegant" -> {"is_checkable":false,"claim_type":"unknown","confidence":0.0,"needs_clarification":true}"#;

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

    /// Try the LLM; `None` on any failure (caller falls back to regex).
    /// Set `TRUTH_LLM_DEBUG=1` to log the raw request/response to stderr.
    pub fn try_extract(&self, text: &str) -> Option<StructuredClaim> {
        // First attempt requests strict JSON mode (supported by OpenAI/llama.cpp);
        // if the server rejects that field (some don't), retry without it.
        let content = self.request(text, true).or_else(|| self.request(text, false));
        if std::env::var("TRUTH_LLM_DEBUG").is_ok() {
            eprintln!("[llm] {} {} -> {:?}", self.base_url, self.model, content);
        }
        parse_claim(&content?)
    }

    fn request(&self, text: &str, json_mode: bool) -> Option<String> {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let mut body = serde_json::json!({
            "model": self.model,
            "temperature": 0,
            "messages": [
                {"role": "system", "content": SYSTEM_PROMPT},
                {"role": "user", "content": text}
            ]
        });
        if json_mode {
            body["response_format"] = serde_json::json!({"type": "json_object"});
        }

        let agent = ureq::AgentBuilder::new().timeout(self.timeout).build();
        let mut req = agent.post(&url).set("content-type", "application/json");
        if let Some(key) = &self.api_key {
            req = req.set("authorization", &format!("Bearer {key}"));
        }
        let resp = req.send_json(body).ok()?;
        let v: serde_json::Value = resp.into_json().ok()?;
        v["choices"][0]["message"]["content"].as_str().map(String::from)
    }
}

/// Parse a `StructuredClaim` from a model response, tolerating markdown fences
/// and surrounding prose by extracting the first balanced `{...}` object.
fn parse_claim(content: &str) -> Option<StructuredClaim> {
    if let Ok(c) = serde_json::from_str::<StructuredClaim>(content.trim()) {
        return Some(c);
    }
    let json = extract_json_object(content)?;
    serde_json::from_str::<StructuredClaim>(&json).ok()
}

/// Find the first balanced top-level `{...}` in a string (ignores ```json fences
/// and chatter around it).
fn extract_json_object(s: &str) -> Option<String> {
    let start = s.find('{')?;
    let bytes = s.as_bytes();
    let mut depth = 0i32;
    let mut in_str = false;
    let mut esc = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_str {
            match b {
                b'\\' if !esc => esc = true,
                b'"' if !esc => in_str = false,
                _ => esc = false,
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(s[start..=i].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

impl ClaimExtractor for OpenAiCompatibleExtractor {
    /// Extract via LLM, falling back to the deterministic extractor.
    fn extract(&self, text: &str) -> StructuredClaim {
        self.try_extract(text)
            .unwrap_or_else(|| RegexExtractor.extract(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_json_from_markdown_fence() {
        let resp = "```json\n{\"is_checkable\":true,\"claim_type\":\"config_value\",\"subject\":\"PARKED\",\"operator\":\"equals\",\"value\":1,\"confidence\":0.9,\"needs_clarification\":false}\n```";
        let c = parse_claim(resp).expect("parse");
        assert_eq!(c.subject.as_deref(), Some("PARKED"));
    }

    #[test]
    fn extracts_json_with_surrounding_prose() {
        let resp = "Sure! Here is the claim:\n{\"is_checkable\":true,\"claim_type\":\"usage_count\",\"subject\":\"JoinHandle\",\"operator\":\"equals\",\"value\":0,\"confidence\":0.8,\"needs_clarification\":false} Hope that helps.";
        let c = parse_claim(resp).expect("parse");
        assert_eq!(c.subject.as_deref(), Some("JoinHandle"));
        assert_eq!(c.expected_number(), Some(0.0));
    }

    #[test]
    fn handles_braces_inside_strings() {
        let resp = "{\"is_checkable\":false,\"claim_type\":\"unknown\",\"subject\":\"a {weird} value\",\"confidence\":0.0,\"needs_clarification\":true,\"operator\":\"unknown\"}";
        let c = parse_claim(resp).expect("parse");
        assert_eq!(c.subject.as_deref(), Some("a {weird} value"));
    }
}
