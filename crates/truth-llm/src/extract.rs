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
    named_const: Regex,
    symbol: Regex,
    symbol_post: Regex,
}

fn res() -> &'static Re {
    static R: OnceLock<Re> = OnceLock::new();
    R.get_or_init(|| Re {
        // A path-like token: /foo, /v1/checkout, /a/b-c
        route: Regex::new(r"(/[A-Za-z0-9_][A-Za-z0-9_/\-.]*)").unwrap(),
        // "port 8080", "port is 8080", "port = 8080", "port: 8080".
        port: Regex::new(r"(?i)port\s*(?:is|=|:|of)?\s*(\d{2,5})").unwrap(),
        retry: Regex::new(r"(?i)retr(?:y|ies|ying)[^0-9]{0,20}(\d+)").unwrap(),
        timeout: Regex::new(r"(?i)timeout[^0-9]{0,20}(\d+)").unwrap(),
        env_var: Regex::new(r"\b([A-Z][A-Z0-9_]{2,})\b").unwrap(),
        // A named constant claim: "MAX_RETRIES is 5", "MaxConns = 10",
        // "DEFAULT_PORT equals 8080". Captures (name, value).
        named_const: Regex::new(
            r"\b([A-Z][A-Za-z0-9]*(?:_[A-Za-z0-9]+)+|[A-Z][a-z0-9]+(?:[A-Z][a-z0-9]+)+|[A-Z][A-Z0-9_]{2,})\b\s*(?:is|=|==|equals|set to|of)\s*(\d+)",
        )
        .unwrap(),
        // Symbol claim, kind-FIRST: "function validate_token", "struct Foo",
        // "the parse_legacy helper" → wait, that's name-first; see symbol_post.
        // This one: KIND then NAME, e.g. "function validate_token".
        symbol: Regex::new(
            r"(?i)\b(?:function|func|fn|method|struct|type|class|interface|trait|enum|helper|handler)\s+`?([A-Za-z_][A-Za-z0-9_]*)`?(?:\s*\(\s*\))?",
        )
        .unwrap(),
        // Symbol claim, NAME-first: "validate_token function", "parse_legacy
        // helper", "handleClick method". Tried only if kind-first didn't match,
        // so it can't steal a word from "<verb> function <name>".
        symbol_post: Regex::new(
            r"(?i)\b`?([A-Za-z_][A-Za-z0-9_]*)`?(?:\s*\(\s*\))?\s+(?:function|func|fn|method|struct|type|class|interface|trait|enum|helper|handler)\b",
        )
        .unwrap(),
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
                let expects_zero = lower.contains("nobody")
                    || lower.contains("no one")
                    || lower.contains("unused");
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
                    if expects_absent {
                        ClaimOperator::NotExists
                    } else {
                        ClaimOperator::Exists
                    },
                    None,
                    None,
                    None,
                    0.7,
                );
            }
        }

        // Error fixed: "X errors are fixed", "the bug is fixed"
        if (lower.contains("fixed") || lower.contains("resolved") || lower.contains("no longer"))
            && (lower.contains("error")
                || lower.contains("bug")
                || lower.contains("issue")
                || lower.contains("webhook"))
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

        // Symbol claim: "I added function validate_token", "removed the
        // parse_legacy helper", "renamed OldClient", "method handleClick()".
        // Requires a kind-word (function/method/struct/...) so we don't grab
        // arbitrary nouns. Skips claims with a /path (those are routes).
        if Self::first_route(text).is_none() {
            // Reject articles / verbs / filler that are never symbol names.
            const NOT_SYMBOL: &[&str] = &[
                "the", "a", "an", "this", "that", "it", "new", "old", "my", "our", "their", "its",
                "some", "any", "no", "added", "removed", "deleted", "renamed", "created",
                "dropped", "wired", "made", "is", "was", "exists", "exist", "ex", "still",
                "present", "called", "named",
            ];
            let ok = |m: regex::Match| {
                let s = m.as_str().to_string();
                (!NOT_SYMBOL.contains(&s.to_ascii_lowercase().as_str())).then_some(s)
            };
            // Try BOTH orders; keep the first that yields a non-stopword name.
            // (Kind-first "function exists" yields the verb "exists" → rejected →
            // name-first "existing_helper function" wins.)
            let name = r
                .symbol
                .captures(text)
                .and_then(|c| c.get(1))
                .and_then(ok)
                .or_else(|| {
                    r.symbol_post
                        .captures(text)
                        .and_then(|c| c.get(1))
                        .and_then(ok)
                });
            // Guard against vague prose ("refactored the checkout handler to be
            // cleaner"): only treat this as a checkable symbol claim when there's
            // a concrete signal — an action/existence verb, or a backticked name.
            // Otherwise it's commentary, and we refuse rather than guess.
            let has_symbol_signal = lower.contains("added")
                || lower.contains("removed")
                || lower.contains("deleted")
                || lower.contains("renamed")
                || lower.contains("created")
                || lower.contains("dropped")
                || lower.contains("exist")
                || lower.contains("defined")
                || lower.contains('`');
            {
                if let Some(name) = name.filter(|_| has_symbol_signal) {
                    let removed = lower.contains("removed")
                        || lower.contains("deleted")
                        || lower.contains("dropped")
                        || lower.contains("renamed")
                        || lower.contains("no longer")
                        || lower.contains("gone");
                    return StructuredClaim {
                        is_checkable: true,
                        claim_type: ClaimType::SymbolExists,
                        subject: Some(name),
                        predicate: Some("symbol_exists".into()),
                        operator: if removed {
                            ClaimOperator::NotExists
                        } else {
                            ClaimOperator::Exists
                        },
                        value: None,
                        unit: None,
                        time_window: Some("recent".into()),
                        environment: None,
                        confidence: 0.74,
                        needs_clarification: false,
                        clarification_question: None,
                    };
                }
            }
        }

        // Named constant value: "MAX_RETRIES is 5", "MaxConns = 10". Keyed by the
        // constant's own name so it resolves against the indexed constant. Runs
        // after the specific port/retry/timeout handlers so those win their
        // dedicated phrasings. Skips a leading /path so route claims aren't eaten.
        if Self::first_route(text).is_none() {
            if let Some(c) = r.named_const.captures(text) {
                let name = c[1].to_string();
                let n: i64 = c[2].parse().unwrap_or_default();
                // Subject and predicate are both the constant's own name so it
                // resolves against the indexed constant (keyed by name).
                return StructuredClaim {
                    is_checkable: true,
                    claim_type: ClaimType::ConfigValue,
                    subject: Some(name.clone()),
                    predicate: Some(name),
                    operator: ClaimOperator::Equals,
                    value: Some(n.into()),
                    unit: None,
                    time_window: Some("recent".into()),
                    environment: None,
                    confidence: 0.8,
                    needs_clarification: false,
                    clarification_question: None,
                };
            }
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
        "a",
        "an",
        "the",
        "as",
        "to",
        "of",
        "and",
        "it",
        "is",
        "was",
        "this",
        "that",
        "dependency",
        "dependencies",
        "crate",
        "package",
        "project",
        "projects",
        "we",
        "i",
        "uses",
        "use",
        "using",
        "used",
        "added",
        "adds",
        "depends",
        "depend",
        "library",
        "lib",
    ];
    let tokens: Vec<&str> = text
        .split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-')
        .filter(|t| !t.is_empty())
        .collect();
    let lower_tokens: Vec<String> = tokens.iter().map(|t| t.to_ascii_lowercase()).collect();

    for i in 0..lower_tokens.len() {
        if AFTER.contains(&lower_tokens[i].as_str()) {
            // Scan forward to the next non-stopword token.
            for cand in lower_tokens.iter().skip(i + 1) {
                if STOP.contains(&cand.as_str()) {
                    continue;
                }
                // Plausible package: lowercase-ish identifier, not all-caps const.
                if cand.len() >= 2
                    && cand
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
                {
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
    fn extracts_port_with_is_phrasing() {
        // "port is 8080" / "port = 8080" must parse, not just "port 8080".
        for s in ["the port is 8080", "port = 8080", "it binds port: 9090"] {
            let c = RegexExtractor.extract(s);
            assert_eq!(c.claim_type, ClaimType::ConfigValue, "{s}");
            assert!(c.expected_number().is_some(), "{s}");
        }
    }

    #[test]
    fn extracts_named_constant() {
        // UPPER_SNAKE and Go-style CamelCase constants, "is"/"="/"set to".
        // (Names containing retry/timeout are claimed by those dedicated
        // handlers first — by design — so use neutral names here.)
        let c = RegexExtractor.extract("MAX_BATCH_SIZE is 16");
        assert_eq!(c.claim_type, ClaimType::ConfigValue);
        assert_eq!(c.subject.as_deref(), Some("MAX_BATCH_SIZE"));
        assert_eq!(c.expected_number(), Some(16.0));

        let g = RegexExtractor.extract("MaxConns = 10");
        assert_eq!(g.claim_type, ClaimType::ConfigValue);
        assert_eq!(g.subject.as_deref(), Some("MaxConns"));
        assert_eq!(g.expected_number(), Some(10.0));
    }

    #[test]
    fn extracts_symbol_claims_both_word_orders() {
        // kind-first
        let a = RegexExtractor.extract("I added function validate_token");
        assert_eq!(a.claim_type, ClaimType::SymbolExists);
        assert_eq!(a.subject.as_deref(), Some("validate_token"));
        // name-first, and "function exists" must NOT capture the verb "exists"
        let b = RegexExtractor.extract("the existing_helper function exists");
        assert_eq!(b.claim_type, ClaimType::SymbolExists);
        assert_eq!(b.subject.as_deref(), Some("existing_helper"));
        // removal sets NotExists
        let c = RegexExtractor.extract("I removed the parse_legacy helper");
        assert_eq!(c.claim_type, ClaimType::SymbolExists);
        assert_eq!(c.subject.as_deref(), Some("parse_legacy"));
        assert_eq!(c.operator, ClaimOperator::NotExists);
    }

    #[test]
    fn vague_symbol_prose_is_not_a_claim() {
        // No action/existence verb, no backticks → commentary, must refuse.
        let c = RegexExtractor.extract("I refactored the checkout handler to be much cleaner");
        assert!(c.claim_type != ClaimType::SymbolExists);
    }

    #[test]
    fn extracts_dependency_name_not_cue_word() {
        // "the project uses serde" must yield `serde`, not `uses`/`project`.
        let c = RegexExtractor.extract("the project uses serde");
        assert_eq!(c.claim_type, ClaimType::DependencyUsed);
        assert_eq!(c.subject.as_deref(), Some("serde"));
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
