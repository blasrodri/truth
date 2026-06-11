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
    file_path: Regex,
    renamed: Regex,
    change_count: Regex,
    from_to: Regex,
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
        // "DEFAULT_PORT equals 8080", "changed MAX_RETRIES from 3 to 5",
        // "bumped MAX_CONNS to 10". The from→to alternative must precede the
        // bare "to" so the POST-change value is captured. Captures (name, value).
        named_const: Regex::new(
            r"\b([A-Z][A-Za-z0-9]*(?:_[A-Za-z0-9]+)+|[A-Z][a-z0-9]+(?:[A-Z][a-z0-9]+)+|[A-Z][A-Z0-9_]{2,})\b\s*(?:is|=|==|equals|set to|of|from\s+\d+\s*[a-z]{0,8}\s+to|to)\s*(\d+)",
        )
        .unwrap(),
        // A file-path token with a real extension: "src/auth.rs", "auth.rs",
        // "docker-compose.yml". The extension must start with a letter so
        // version numbers ("3.5") don't match.
        file_path: Regex::new(r"\b((?:[\w.-]+/)*[\w-][\w.-]*\.[A-Za-z][A-Za-z0-9]{0,7})\b")
            .unwrap(),
        // "renamed X to Y" (optionally "renamed the X function to Y").
        renamed: Regex::new(
            r"(?i)\brenam(?:ed|es|e|ing)\s+(?:the\s+)?`?([A-Za-z_/][\w/.\-]*)`?(?:\s+(?:function|func|fn|method|struct|class|type|helper|handler|variable|field|route|endpoint))?\s+to\s+`?([A-Za-z_/][\w/.\-]*)`?",
        )
        .unwrap(),
        // "updated all 4 call sites (of X)" / "fixed 3 occurrences of Y".
        change_count: Regex::new(
            r"(?i)\b(?:all\s+)?(\d+)\s+(?:call\s?-?sites?|occurrences?|usages?|references?|places)\b(?:\s+of\s+(?:the\s+)?`?([\w/.\-]+)`?)?",
        )
        .unwrap(),
        // "from 3 to 5" / "from 10s to 30s" — the claimed POST-change value is
        // the second number, not the first one a generic capture would grab.
        from_to: Regex::new(r"(?i)\bfrom\s+(\d+)\s*[a-z]{0,8}\s+to\s+(\d+)").unwrap(),
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

        // Out-of-scope claims: truth's evidence is the LOCAL repo (index,
        // working-tree diff, receipts). Claims about what a URL serves or
        // what happened on another machine are unverifiable here BY DESIGN —
        // judging them against local evidence contradicted true statements
        // about a remote deployment (caught in the wild). Refuse early.
        if text.contains("://") || has_remote_marker(&lower) {
            return StructuredClaim::unknown(
                "This claim is about external state (a URL or another machine) — truth only verifies the local repo, diff, and recorded runs.",
            );
        }

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

        // Command success: "tests pass", "the build compiles", "clippy is
        // clean". Decided from recorded command receipts, never from prose —
        // so extraction only needs the command KIND. Bare "I ran the tests"
        // (no success assertion) stays unverifiable by design, and admissions
        // of failure are not extracted (nothing to catch).
        if let Some(kind) = command_kind(&lower) {
            return claim(
                ClaimType::CommandSucceeded,
                Some(kind.to_string()),
                Some("command_succeeded"),
                ClaimOperator::Exists,
                None,
                None,
                None,
                0.8,
            );
        }

        // Scope claim: "I only changed src/auth.rs" / "only touched the
        // `parser`". Needs a concrete subject (path or backticked token) —
        // "only changed the error message" is too vague to scope-check and
        // falls through to refusal.
        let only_phrasing = lower.contains("only ")
            && ["changed", "touched", "modified", "edited", "updated"]
                .iter()
                .any(|v| lower.contains(v))
            && !lower.contains("call site");
        if only_phrasing && !has_value_pattern(text, &lower) {
            if let Some(subject) = file_subject(text).or_else(|| backticked(text)) {
                return claim(
                    ClaimType::OnlyChanged,
                    Some(subject),
                    Some("only_changed"),
                    ClaimOperator::Exists,
                    None,
                    None,
                    None,
                    0.78,
                );
            }
        }

        // Rename claim: "I renamed parse_legacy to parse_v2". Both names are
        // captured; the verdict needs the old name gone AND the new one added.
        if let Some(c) = r.renamed.captures(text) {
            let old = c[1].to_string();
            let new = c[2].to_string();
            if !is_stopword(&old) && !is_stopword(&new) {
                return StructuredClaim {
                    is_checkable: true,
                    claim_type: ClaimType::SymbolRenamed,
                    subject: Some(old),
                    predicate: Some("renamed".into()),
                    operator: ClaimOperator::Exists,
                    value: Some(serde_json::json!({ "to": new })),
                    unit: None,
                    time_window: Some("recent".into()),
                    environment: None,
                    confidence: 0.78,
                    needs_clarification: false,
                    clarification_question: None,
                };
            }
        }

        // Change-count claim: "updated all 4 call sites of parse_config".
        // Without a concrete subject there is nothing to count against, so it
        // falls through to refusal.
        if let Some(c) = r.change_count.captures(text) {
            let n: i64 = c[1].parse().unwrap_or_default();
            let subject = c
                .get(2)
                .map(|m| m.as_str().to_string())
                .or_else(|| backticked(text));
            if let Some(subject) = subject {
                return claim(
                    ClaimType::ChangeCount,
                    Some(subject),
                    Some("change_count"),
                    ClaimOperator::Equals,
                    Some(n.into()),
                    Some("sites"),
                    None,
                    0.72,
                );
            }
        }

        // File-change claim: "I modified src/auth.rs", "created tests/foo.rs",
        // "deleted old_config.toml". Needs a dot-extension path token so HTTP
        // routes ("/v1/refund") never land here; skipped when the sentence
        // also carries a pure route or a value claim (those are more specific).
        if !has_pure_route(text) && !has_value_pattern(text, &lower) {
            if let Some(path) = file_subject(text) {
                let expected = if ["deleted", "removed", "dropped"]
                    .iter()
                    .any(|v| lower.contains(v))
                {
                    Some("deleted")
                } else if lower.contains("created")
                    || lower.contains("new file")
                    || lower.contains("added")
                {
                    Some("added")
                } else if [
                    "modified",
                    "edited",
                    "updated",
                    "changed",
                    "touched",
                    "rewrote",
                    "tweaked",
                    "refactored",
                ]
                .iter()
                .any(|v| lower.contains(v))
                {
                    Some("modified")
                } else {
                    None
                };
                if let Some(expected) = expected {
                    return claim(
                        ClaimType::FileChanged,
                        Some(path),
                        Some("file_changed"),
                        if expected == "deleted" {
                            ClaimOperator::NotExists
                        } else {
                            ClaimOperator::Exists
                        },
                        Some(expected.into()),
                        None,
                        None,
                        0.8,
                    );
                }
            }
        }

        // Dependency: "added serde as a dependency", "depends on X", "the X
        // crate/package". The subject is a bare package token (not a /path).
        // Bare "uses X" is deliberately NOT a cue: it fires on any prose about
        // usage ("nobody uses it" once yielded the package `contradicted`) —
        // a dependency claim must name the dependency relationship.
        let dep_phrasing = lower.contains("dependency")
            || lower.contains("depends on")
            || lower.contains("depend on")
            || lower.contains(" crate")
            || lower.contains(" package")
            || lower.contains(" library");
        if dep_phrasing && !has_pure_route(text) {
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

        // Retry count: "we retry payments 3 times", "changed retries from 3 to
        // 5" (a from→to phrasing claims the SECOND number as the new state).
        if let Some(c) = r.retry.captures(&lower) {
            let n: i64 =
                post_change_target(&lower).unwrap_or_else(|| c[1].parse().unwrap_or_default());
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
            let n: i64 =
                post_change_target(&lower).unwrap_or_else(|| c[1].parse().unwrap_or_default());
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
            let n: i64 =
                post_change_target(&lower).unwrap_or_else(|| c[1].parse().unwrap_or_default());
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
        // arbitrary nouns. Skips claims with a pure /path (those are routes) —
        // a file-path mention ("in src/auth.rs") doesn't suppress it.
        if !has_pure_route(text) {
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
            // Name-first matches must additionally LOOK like identifiers
            // (snake_case, camelCase, a digit, or backticked). Plain prose
            // adjectives otherwise become "symbols": "the timing-safe helper
            // exists" once extracted the fragment `safe` and contradicted a
            // true sentence — the engine judged a name the extractor invented.
            let identifier_like = |m: &regex::Match| {
                let s = m.as_str();
                let before = text[..m.start()].chars().next_back();
                before != Some('-')
                    && (s.contains('_')
                        || s.chars().any(|c| c.is_ascii_digit())
                        || (s.chars().any(|c| c.is_ascii_uppercase())
                            && s.chars().any(|c| c.is_ascii_lowercase()))
                        || text.contains(&format!("`{s}`")))
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
                        .filter(identifier_like)
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
        // dedicated phrasings. Skips a pure /path so route claims aren't eaten.
        if !has_pure_route(text) {
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

/// "from 3 to 5" phrasing: the claimed POST-change value is the second number.
fn post_change_target(lower: &str) -> Option<i64> {
    res()
        .from_to
        .captures(lower)
        .and_then(|c| c[2].parse().ok())
}

/// Match "tests pass" / "the build compiles" / "clippy is clean" phrasings to
/// a receipt kind. The success word must FOLLOW the command word so "I added a
/// passing test" (about a test file, not the suite) doesn't match. Admissions
/// of failure are never extracted — there is nothing to catch.
fn command_kind(lower: &str) -> Option<&'static str> {
    static K: OnceLock<Vec<(Regex, &'static str)>> = OnceLock::new();
    let pats = K.get_or_init(|| {
        vec![
            (
                Regex::new(r"\b(?:tests?|test suite|suite|specs?|checks?)\s+(?:are\s+|all\s+|now\s+|still\s+)*(?:pass(?:es|ed|ing)?\b|green\b|succeed)").unwrap(),
                "test",
            ),
            (
                Regex::new(r"\b(?:build|compilation)\s+(?:now\s+|still\s+|is\s+)*(?:succeed(?:s|ed)?|pass(?:es|ed)?|compiles|works|green|clean)").unwrap(),
                "build",
            ),
            (
                Regex::new(r"\b(?:it|everything|the project|the crate|the code|the workspace)\s+(?:now\s+|still\s+|all\s+)*compiles\b").unwrap(),
                "build",
            ),
            (
                Regex::new(r"\b(?:clippy|lint(?:er|ing)?)\s+(?:is\s+|now\s+|still\s+)*(?:clean|pass(?:es|ed)?|green|happy)").unwrap(),
                "lint",
            ),
            (
                Regex::new(r"\b(?:typecheck(?:ing)?|type[- ]check(?:s|ing)?|tsc)\s+(?:is\s+|now\s+)*(?:pass(?:es|ed)?|clean|green)").unwrap(),
                "typecheck",
            ),
        ]
    });
    if lower.contains("fail") {
        return None;
    }
    pats.iter()
        .find(|(re, _)| re.is_match(lower))
        .map(|(_, k)| *k)
}

/// First file-path token (dot-extension required) in the text.
fn file_subject(text: &str) -> Option<String> {
    res().file_path.captures(text).map(|c| c[1].to_string())
}

/// First `backticked` token, if short enough to be a subject.
fn backticked(text: &str) -> Option<String> {
    let start = text.find('`')?;
    let rest = &text[start + 1..];
    let end = rest.find('`')?;
    let token = rest[..end].trim();
    (!token.is_empty() && token.len() <= 80).then(|| token.to_string())
}

/// Any route-regex hit whose last segment has NO dot-extension — i.e. an HTTP
/// path like `/v1/refund`, not a file path like `src/auth.rs`.
fn has_pure_route(text: &str) -> bool {
    res().route.captures_iter(text).any(|c| {
        let last = c[1].rsplit('/').next().unwrap_or("");
        !last.contains('.')
    })
}

/// Whether a more specific value claim (constant/retry/timeout/port) is present
/// — those handlers must win over file/scope claims for sentences like
/// "updated MAX_RETRIES to 5 in src/config.rs".
fn has_value_pattern(text: &str, lower: &str) -> bool {
    let r = res();
    r.named_const.is_match(text)
        || r.retry.is_match(lower)
        || r.timeout.is_match(lower)
        || r.port.is_match(lower)
}

/// Whether the sentence is about another machine/environment rather than the
/// local working tree — deploys, ssh/ssm sessions, containers, "the box".
fn has_remote_marker(lower: &str) -> bool {
    [
        "on the server",
        "on the box",
        "on the host",
        "via ssh",
        "via ssm",
        "over ssh",
        "deployed",
        "in production",
        "on production",
        "the container",
        "remote machine",
        "remote box",
        "remote server",
    ]
    .iter()
    .any(|m| lower.contains(m))
}

/// Words that are never symbol/file names in a rename claim.
fn is_stopword(token: &str) -> bool {
    const STOP: &[&str] = &[
        "the", "a", "an", "this", "that", "it", "new", "old", "my", "our", "their", "its", "some",
        "any", "no", "is", "was", "to", "from",
    ];
    STOP.contains(&token.to_ascii_lowercase().as_str())
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
    fn prose_adjectives_are_not_symbol_names() {
        // "the timing-safe helper exists" once extracted the fragment `safe`
        // as a symbol and contradicted a TRUE sentence (caught in the wild).
        // Name-first symbol names must look like identifiers.
        let c = RegexExtractor.extract("confirm the timing-safe helper exists");
        assert_ne!(c.claim_type, ClaimType::SymbolExists, "{:?}", c.subject);

        // Identifier-shaped names still extract: snake_case, camelCase,
        // backticked plain words.
        for text in [
            "the timing_safe_eq helper exists",
            "the handleClick handler exists",
            "the `compare` helper exists",
        ] {
            let c = RegexExtractor.extract(text);
            assert_eq!(c.claim_type, ClaimType::SymbolExists, "{text}");
        }
    }

    #[test]
    fn extracts_dependency_name_not_cue_word() {
        // "uses the serde crate" must yield `serde`, not `uses`/`crate`.
        let c = RegexExtractor.extract("the project uses the serde crate");
        assert_eq!(c.claim_type, ClaimType::DependencyUsed);
        assert_eq!(c.subject.as_deref(), Some("serde"));

        let d = RegexExtractor.extract("we depend on tokio");
        assert_eq!(d.claim_type, ClaimType::DependencyUsed);
        assert_eq!(d.subject.as_deref(), Some("tokio"));
    }

    #[test]
    fn bare_uses_prose_is_not_a_dependency_claim() {
        // '"nobody uses it" was contradicted by the route existing' once
        // extracted the package `contradicted`. Bare "uses" without a
        // dependency word must not produce a dependency claim.
        let c = RegexExtractor.extract("\"nobody uses it\" was contradicted by the route existing");
        assert_ne!(c.claim_type, ClaimType::DependencyUsed);
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

    // ---- diff-native claim family ----------------------------------------

    #[test]
    fn extracts_file_changed_claims() {
        let m = RegexExtractor.extract("I modified src/auth.rs");
        assert_eq!(m.claim_type, ClaimType::FileChanged);
        assert_eq!(m.subject.as_deref(), Some("src/auth.rs"));
        assert_eq!(m.value, Some("modified".into()));

        let a = RegexExtractor.extract("created tests/foo_test.rs");
        assert_eq!(a.claim_type, ClaimType::FileChanged);
        assert_eq!(a.value, Some("added".into()));

        let d = RegexExtractor.extract("I deleted old_config.toml");
        assert_eq!(d.claim_type, ClaimType::FileChanged);
        assert_eq!(d.operator, ClaimOperator::NotExists);
        assert_eq!(d.value, Some("deleted".into()));
    }

    #[test]
    fn route_claims_still_win_over_file_claims() {
        // A pure HTTP route in the sentence keeps it a route claim even when a
        // file path is also mentioned.
        let c = RegexExtractor.extract("I added the /v1/refund endpoint in src/routes.rs");
        assert_eq!(c.claim_type, ClaimType::RouteExists);
        assert_eq!(c.subject.as_deref(), Some("/v1/refund"));
    }

    #[test]
    fn value_claims_win_over_file_claims() {
        // "updated X to 5 in file.rs" is a value claim, not a file claim.
        // (A retry-flavored name like MAX_RETRIES routes to the dedicated
        // RetryCount handler instead — also a value claim, also fine.)
        let c = RegexExtractor.extract("updated MAX_CONNS to 16 in src/config.rs");
        assert_eq!(c.claim_type, ClaimType::ConfigValue);
        assert_eq!(c.subject.as_deref(), Some("MAX_CONNS"));
        assert_eq!(c.expected_number(), Some(16.0));

        let r = RegexExtractor.extract("updated MAX_RETRIES to 5 in src/config.rs");
        assert_eq!(r.claim_type, ClaimType::RetryCount);
        assert_eq!(r.expected_number(), Some(5.0));
    }

    #[test]
    fn extracts_only_changed_scope_claim() {
        let c = RegexExtractor.extract("I only changed src/auth.rs");
        assert_eq!(c.claim_type, ClaimType::OnlyChanged);
        assert_eq!(c.subject.as_deref(), Some("src/auth.rs"));

        // Backticked module subject works too.
        let m = RegexExtractor.extract("I only touched the `parser` module");
        assert_eq!(m.claim_type, ClaimType::OnlyChanged);
        assert_eq!(m.subject.as_deref(), Some("parser"));

        // Vague scope claims refuse rather than guess.
        let v = RegexExtractor.extract("I only changed the error message");
        assert_ne!(v.claim_type, ClaimType::OnlyChanged);
    }

    #[test]
    fn extracts_rename_claim() {
        let c = RegexExtractor.extract("I renamed parse_legacy to parse_v2");
        assert_eq!(c.claim_type, ClaimType::SymbolRenamed);
        assert_eq!(c.subject.as_deref(), Some("parse_legacy"));
        assert_eq!(
            c.value
                .as_ref()
                .and_then(|v| v.get("to"))
                .and_then(|v| v.as_str()),
            Some("parse_v2")
        );

        // With a kind word in the middle.
        let k = RegexExtractor.extract("renamed the old_client struct to NewClient");
        assert_eq!(k.claim_type, ClaimType::SymbolRenamed);
        assert_eq!(k.subject.as_deref(), Some("old_client"));
    }

    #[test]
    fn extracts_change_count_claim() {
        let c = RegexExtractor.extract("updated all 4 call sites of parse_config");
        assert_eq!(c.claim_type, ClaimType::ChangeCount);
        assert_eq!(c.subject.as_deref(), Some("parse_config"));
        assert_eq!(c.expected_number(), Some(4.0));

        // No subject → nothing to count against → refused.
        let v = RegexExtractor.extract("updated all 4 call sites");
        assert!(!v.is_checkable);
    }

    #[test]
    fn from_to_phrasing_claims_the_post_change_value() {
        let r = RegexExtractor.extract("I changed retries from 3 to 5");
        assert_eq!(r.claim_type, ClaimType::RetryCount);
        assert_eq!(r.expected_number(), Some(5.0));

        let t = RegexExtractor.extract("bumped the timeout from 10 to 30");
        assert_eq!(t.claim_type, ClaimType::TimeoutValue);
        assert_eq!(t.expected_number(), Some(30.0));

        let c = RegexExtractor.extract("changed MAX_CONNS from 8 to 16");
        assert_eq!(c.claim_type, ClaimType::ConfigValue);
        assert_eq!(c.expected_number(), Some(16.0));
    }

    #[test]
    fn extracts_command_success_claims() {
        for (text, kind) in [
            ("tests pass", "test"),
            ("the test suite passes", "test"),
            ("all tests are passing", "test"),
            ("the build compiles", "build"),
            ("it compiles", "build"),
            ("clippy is clean", "lint"),
        ] {
            let c = RegexExtractor.extract(text);
            assert_eq!(c.claim_type, ClaimType::CommandSucceeded, "{text}");
            assert_eq!(c.subject.as_deref(), Some(kind), "{text}");
        }
    }

    #[test]
    fn external_state_claims_are_refused_not_judged() {
        // truth's evidence is the LOCAL repo. Claims about what a URL serves
        // or what happened on another machine contradicted TRUE statements
        // about a remote deployment (caught in the wild) — they must refuse.
        for text in [
            "https://prode.example.com/js/admin.js returns the new file with 0 inline onclick",
            "only public/ files changed on the server",
            "I deployed the fix and only touched public/assets.js",
            "I updated config.toml on the box via ssm",
        ] {
            let c = RegexExtractor.extract(text);
            assert!(!c.is_checkable, "{text} → {:?}", c.claim_type);
        }
        // Local-tree claims still extract normally.
        let local = RegexExtractor.extract("I only changed src/auth.rs");
        assert!(local.is_checkable);
    }

    #[test]
    fn command_claims_require_success_after_command_word() {
        // About a test FILE, not the suite's status.
        let c = RegexExtractor.extract("I added a passing test for the parser");
        assert_ne!(c.claim_type, ClaimType::CommandSucceeded);
        // Admission of failure is not a claim to catch.
        let f = RegexExtractor.extract("tests fail right now");
        assert_ne!(f.claim_type, ClaimType::CommandSucceeded);
    }
}
