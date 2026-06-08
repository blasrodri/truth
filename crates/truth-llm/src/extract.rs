//! Claim extraction. The default path is a deterministic regex/keyword
//! extractor so the whole pipeline runs offline; an optional OpenAI-compatible
//! extractor can be layered in front, falling back here on any error.

use regex::Regex;
use std::sync::OnceLock;
use truth_core::claim::{ClaimOperator, ClaimType, StructuredClaim};

pub trait ClaimExtractor {
    fn extract(&self, text: &str) -> StructuredClaim;
}

/// Deterministic, offline claim extractor.
pub struct RegexExtractor;

struct Re {
    route: Regex,
    port: Regex,
    retry: Regex,
    timeout: Regex,
    env_var: Regex,
}

fn res() -> &'static Re {
    static R: OnceLock<Re> = OnceLock::new();
    R.get_or_init(|| Re {
        // A path-like token: /foo, /v1/checkout, /a/b-c
        route: Regex::new(r"(/[A-Za-z0-9_][A-Za-z0-9_/\-.]*)").unwrap(),
        port: Regex::new(r"(?i)port\s+(\d{2,5})").unwrap(),
        retry: Regex::new(r"(?i)retr(?:y|ies|ying)[^0-9]{0,20}(\d+)").unwrap(),
        timeout: Regex::new(r"(?i)timeout[^0-9]{0,20}(\d+)").unwrap(),
        env_var: Regex::new(r"\b([A-Z][A-Z0-9_]{2,})\b").unwrap(),
    })
}

impl RegexExtractor {
    fn first_route(text: &str) -> Option<String> {
        res().route.captures(text).map(|c| c[1].to_string())
    }
}

impl ClaimExtractor for RegexExtractor {
    fn extract(&self, text: &str) -> StructuredClaim {
        let lower = text.to_ascii_lowercase();
        let r = res();

        // Usage: "nobody/no one uses X", "does anyone still use X", "X is unused"
        let usage_phrasing = lower.contains("nobody use")
            || lower.contains("no one use")
            || lower.contains("nobody uses")
            || lower.contains("no one uses")
            || lower.contains("still use")
            || lower.contains("anyone use")
            || lower.contains("unused")
            || lower.contains("receive traffic")
            || lower.contains("receives traffic");
        if usage_phrasing {
            if let Some(route) = Self::first_route(text) {
                let expects_zero =
                    lower.contains("nobody") || lower.contains("no one") || lower.contains("unused");
                return claim(
                    ClaimType::UsageCount,
                    Some(route),
                    Some("request_count"),
                    if expects_zero {
                        ClaimOperator::Equals
                    } else {
                        ClaimOperator::Unknown
                    },
                    if expects_zero { Some(0.into()) } else { None },
                    Some("requests"),
                    detect_env(&lower),
                    0.82,
                );
            }
        }

        // Dependency: "uses tokio", "added serde as a dependency", "depends on
        // X", "the X crate/package". The subject is a bare package token (not a
        // /path). Gated so route/usage claims above win first.
        let dep_phrasing = lower.contains("dependency")
            || lower.contains("depends on")
            || lower.contains("depend on")
            || lower.contains(" crate")
            || lower.contains(" package")
            || lower.contains("uses ")
            || lower.contains("use the ");
        if dep_phrasing && Self::first_route(text).is_none() {
            if let Some(dep) = dependency_name(text, &lower) {
                let expects_absent = lower.contains("no longer")
                    || lower.contains("removed")
                    || lower.contains("dropped")
                    || lower.contains("not used")
                    || lower.contains("unused");
                return claim(
                    ClaimType::DependencyUsed,
                    Some(dep),
                    Some("dependency"),
                    if expects_absent { ClaimOperator::NotExists } else { ClaimOperator::Exists },
                    None,
                    None,
                    None,
                    0.7,
                );
            }
        }

        // Error fixed: "X errors are fixed", "the bug is fixed"
        if (lower.contains("fixed") || lower.contains("resolved") || lower.contains("no longer"))
            && (lower.contains("error") || lower.contains("bug") || lower.contains("issue") || lower.contains("webhook"))
        {
            let subject = error_subject(&lower);
            return claim(
                ClaimType::ErrorStillHappening,
                subject,
                Some("error_count"),
                ClaimOperator::Equals,
                Some(0.into()),
                None,
                detect_env(&lower),
                0.78,
            );
        }

        // Retry count: "we retry payments 3 times"
        if let Some(c) = r.retry.captures(&lower) {
            let n: i64 = c[1].parse().unwrap_or_default();
            return claim(
                ClaimType::RetryCount,
                Some("retry_count".into()),
                Some("retry_count"),
                ClaimOperator::Equals,
                Some(n.into()),
                Some("times"),
                None,
                0.85,
            );
        }

        // Timeout value
        if let Some(c) = r.timeout.captures(&lower) {
            let n: i64 = c[1].parse().unwrap_or_default();
            return claim(
                ClaimType::TimeoutValue,
                Some("timeout".into()),
                Some("timeout"),
                ClaimOperator::Equals,
                Some(n.into()),
                Some("seconds"),
                None,
                0.8,
            );
        }

        // Port / config value: "runs on port 8080"
        if let Some(c) = r.port.captures(&lower) {
            let n: i64 = c[1].parse().unwrap_or_default();
            return claim(
                ClaimType::ConfigValue,
                Some("port".into()),
                Some("port"),
                ClaimOperator::Equals,
                Some(n.into()),
                None,
                None,
                0.84,
            );
        }

        // Route existence: positive ("the /x route exists", "/x is still
        // registered/wired up/present", "I added the /x endpoint") and negative
        // ("I removed/deleted the /x endpoint"). An agent reporting its own work
        // phrases existence many ways, so match the verbs, not just "exist".
        let mentions_route = lower.contains("route") || lower.contains("endpoint");
        let exists_verb = lower.contains("exist")
            || lower.contains("registered")
            || lower.contains("wired up")
            || lower.contains("still there")
            || lower.contains("still present")
            || lower.contains("added")
            || lower.contains("created")
            || lower.contains("added the")
            || lower.contains("wired");
        let removed_verb = lower.contains("removed")
            || lower.contains("deleted")
            || lower.contains("dropped")
            || lower.contains("no longer exists")
            || lower.contains("gone");
        if (mentions_route || removed_verb || exists_verb) && Self::first_route(text).is_some() {
            let route = Self::first_route(text).unwrap();
            // Negative existence wins when present (e.g. "I removed /x") so the
            // engine checks for ABSENCE; otherwise it's a positive existence claim.
            let (op, conf) = if removed_verb {
                (ClaimOperator::NotExists, 0.78)
            } else if exists_verb {
                (ClaimOperator::Exists, 0.8)
            } else {
                // bare route mention with no verb — weak, but still an existence check
                (ClaimOperator::Exists, 0.55)
            };
            return claim(
                ClaimType::RouteExists,
                Some(route),
                Some("route_exists"),
                op,
                None,
                None,
                None,
                conf,
            );
        }

        // Env var exists
        if lower.contains("env") && (lower.contains("var") || lower.contains("variable")) {
            if let Some(c) = r.env_var.captures(text) {
                return claim(
                    ClaimType::EnvVarExists,
                    Some(c[1].to_string()),
                    Some("env_var_exists"),
                    ClaimOperator::Exists,
                    None,
                    None,
                    None,
                    0.7,
                );
            }
        }

        StructuredClaim::unknown(
            "I couldn't identify a concrete, checkable claim. Try quoting a specific route, value, or error.",
        )
    }
}

/// Pull a likely package name from a dependency claim. Looks for the token
/// after a dependency cue ("uses X", "depends on X", "added X", "the X crate")
/// and returns the first plausible lowercase package identifier.
fn dependency_name(text: &str, lower: &str) -> Option<String> {
    // Cue words after which the next token is the package. Order matters: the
    // specific cues are tried before the generic "the".
    const AFTER: &[&str] = &["uses", "use", "using", "on", "added", "adds", "the"];
    // Words that are never package names in this position (incl. the cue verbs
    // themselves, so "the project uses serde" doesn't return "uses").
    const STOP: &[&str] = &[
        "a", "an", "the", "as", "to", "of", "and", "it", "is", "was", "this",
        "that", "dependency", "dependencies", "crate", "package", "project",
        "projects", "we", "i", "uses", "use", "using", "used", "added", "adds",
        "depends", "depend", "library", "lib",
    ];
    let tokens: Vec<&str> = text.split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-').filter(|t| !t.is_empty()).collect();
    let lower_tokens: Vec<String> = tokens.iter().map(|t| t.to_ascii_lowercase()).collect();

    for i in 0..lower_tokens.len() {
        if AFTER.contains(&lower_tokens[i].as_str()) {
            // Scan forward to the next non-stopword token.
            for j in (i + 1)..lower_tokens.len() {
                let cand = &lower_tokens[j];
                if STOP.contains(&cand.as_str()) {
                    continue;
                }
                // Plausible package: lowercase-ish identifier, not all-caps const.
                if cand.len() >= 2 && cand.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
                    return Some(cand.clone());
                }
                break;
            }
        }
    }
    let _ = lower;
    None
}

fn detect_env(lower: &str) -> Option<String> {
    if lower.contains("prod") {
        Some("prod".into())
    } else if lower.contains("staging") {
        Some("staging".into())
    } else {
        None
    }
}

fn error_subject(lower: &str) -> Option<String> {
    for kw in ["webhook", "payment", "checkout", "login", "billing"] {
        if lower.contains(kw) {
            return Some(kw.to_string());
        }
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn claim(
    claim_type: ClaimType,
    subject: Option<String>,
    predicate: Option<&str>,
    operator: ClaimOperator,
    value: Option<serde_json::Value>,
    unit: Option<&str>,
    environment: Option<String>,
    confidence: f32,
) -> StructuredClaim {
    StructuredClaim {
        is_checkable: true,
        claim_type,
        subject,
        predicate: predicate.map(str::to_string),
        operator,
        value,
        unit: unit.map(str::to_string),
        time_window: Some("recent".into()),
        environment,
        confidence,
        needs_clarification: false,
        clarification_question: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_usage_zero() {
        let c = RegexExtractor.extract("Nobody uses /v1/checkout anymore.");
        assert_eq!(c.claim_type, ClaimType::UsageCount);
        assert_eq!(c.subject.as_deref(), Some("/v1/checkout"));
        assert_eq!(c.expected_number(), Some(0.0));
    }

    #[test]
    fn extracts_retry_count() {
        let c = RegexExtractor.extract("We still retry payments 3 times.");
        assert_eq!(c.claim_type, ClaimType::RetryCount);
        assert_eq!(c.expected_number(), Some(3.0));
    }

    #[test]
    fn extracts_port() {
        let c = RegexExtractor.extract("The service runs on port 8080.");
        assert_eq!(c.claim_type, ClaimType::ConfigValue);
        assert_eq!(c.expected_number(), Some(8080.0));
    }

    #[test]
    fn extracts_error_fixed() {
        let c = RegexExtractor.extract("Webhook errors are fixed.");
        assert_eq!(c.claim_type, ClaimType::ErrorStillHappening);
        assert_eq!(c.subject.as_deref(), Some("webhook"));
    }

    #[test]
    fn unknown_for_vague() {
        let c = RegexExtractor.extract("I think the system is good.");
        assert!(!c.is_checkable);
    }
}
