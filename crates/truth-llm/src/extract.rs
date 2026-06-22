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
        // "port 8080", "port is 8080", "port = 8080", "port: 8080", and the
        // bare "(is) listening on 8080" / "listens on 8080" phrasing (a server
        // listening on N is a port claim). The listen arm needs a 4-5 digit
        // number so it doesn't grab small unrelated counts.
        port: Regex::new(
            r"(?i)(?:port\s*(?:is|=|:|of)?\s*(\d{2,5})|listen(?:s|ing)?\s+on\s+(?:port\s+)?(\d{3,5}))",
        )
        .unwrap(),
        // Two retry phrasings:
        //  (a) close form — a retry word then a number within a short window:
        //      "retry 3", "MAX_RETRIES is 5", "retried 5 times".
        //  (b) "N times" form — a retry/attempt cue with the count right before
        //      "times", tolerating a long gap: "attempt the payment up to 5
        //      times". Spelled numbers (one..twelve) accepted in both.
        // Capture group 1 (close) or 2 (times) holds the count.
        retry: Regex::new(
            r"(?i)(?:retr(?:y|ies|ied|ying)[^0-9]{0,20}(\d+|one|two|three|four|five|six|seven|eight|nine|ten|eleven|twelve)\b|(?:retr(?:y|ies|ied|ying)|attempts?|attempted)\b[^0-9]*?(\d+|one|two|three|four|five|six|seven|eight|nine|ten|eleven|twelve)\s+times?\b)",
        )
        .unwrap(),
        // "timeout 30", "times out after 30", "time out after 30s". The space in
        // "time out", the verb form ("times out"), and a trailing unit ("30s",
        // "30 seconds") are all matched.
        timeout: Regex::new(
            r"(?i)time\s?-?\s?out[^0-9]{0,12}(\d+|one|two|three|four|five|six|seven|eight|nine|ten)",
        )
        .unwrap(),
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
        // NOTE: `field`/`variant`/`const` are intentionally NOT kind-first cues —
        // as plain English words they appear mid-sentence ("field to the struct"
        // → name `to`), so they're recognized name-first only (below).
        symbol: Regex::new(
            r"(?i)\b(?:function|func|fn|method|struct|type|class|interface|trait|enum|helper|handler)\s+`?([A-Za-z_][A-Za-z0-9_]*)`?(?:\s*\(\s*\))?",
        )
        .unwrap(),
        // Symbol claim, NAME-first: "validate_token function", "parse_legacy
        // helper", "handleClick method", "subject field". Tried only if
        // kind-first didn't match, so it can't steal a word from
        // "<verb> function <name>". `field`/`variant`/`const` resolve against
        // the AST member facts added 2026-06-12.
        symbol_post: Regex::new(
            r"(?i)\b`?([A-Za-z_][A-Za-z0-9_]*)`?(?:\s*\(\s*\))?\s+(?:function|func|fn|method|struct|type|class|interface|trait|enum|helper|handler|field|variant|const|constant)\b",
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
        //
        // ALSO skipped when the sentence names a symbol KIND ("added the X
        // function in refs.rs"): there the file is just a LOCATION and the
        // assertion is about the symbol — let the symbol claim (below) win.
        // Without this, "added a dependency_index_populated function in refs.rs"
        // became a file_changed claim on `refs.rs` and contradicted (the file
        // wasn't in the current diff), a residual FP the ledger review surfaced.
        if !has_pure_route(text)
            && !has_value_pattern(text, &lower)
            && !has_symbol_kind_phrase(text)
            && !has_import_phrase(&lower)
        {
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
        // Whole-word cues only: a name that merely CONTAINS "dependency" (e.g.
        // the identifier `dependency_index_populated` in "added a
        // dependency_index_populated field") must NOT read as a dependency
        // claim. Substring matching here mis-typed symbol claims as dep claims.
        // "import"/"imports"/"imported" are dependency cues too: an added import
        // IS a dependency-use claim ("added an os import to fetch.py"). Without
        // this, "added an os import" matched the file_changed branch on the
        // co-mentioned file (fetch.py) and CONTRADICTED when the file pre-existed
        // ("claimed added, but the diff shows fetch.py was modified") — an FP the
        // SWE-bench calibration surfaced on truth's own working tree.
        let dep_phrasing = has_word(&lower, "dependency")
            || has_word(&lower, "dependencies")
            || lower.contains("depends on")
            || lower.contains("depend on")
            || has_word(&lower, "crate")
            || has_word(&lower, "crates")
            || has_word(&lower, "package")
            || has_word(&lower, "library")
            || has_word(&lower, "import")
            || has_word(&lower, "imports")
            || has_word(&lower, "imported");
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

        // Retry count: "we retry payments 3 times", "retried five times",
        // "changed retries from 3 to 5" (from→to claims the SECOND number).
        if let Some(c) = r.retry.captures(&lower) {
            let matched = c
                .get(1)
                .or_else(|| c.get(2))
                .map(|m| m.as_str())
                .unwrap_or("");
            let n: i64 = post_change_target(&lower)
                .or_else(|| parse_count(matched))
                .unwrap_or_default();
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

        // Timeout value ("timeout 30", "times out after 30s", "time out ... ten")
        if let Some(c) = r.timeout.captures(&lower) {
            let n: i64 = post_change_target(&lower)
                .or_else(|| parse_count(c.get(1).map(|m| m.as_str()).unwrap_or("")))
                .unwrap_or_default();
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

        // Port / config value: "runs on port 8080", "listening on 8080". The
        // port arm is group 1, the listen arm group 2 — take whichever matched.
        if let Some(c) = r.port.captures(&lower) {
            let matched = c
                .get(1)
                .or_else(|| c.get(2))
                .map(|m| m.as_str())
                .unwrap_or("");
            let n: i64 = post_change_target(&lower)
                .or_else(|| parse_count(matched))
                .unwrap_or_default();
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
                "the",
                "a",
                "an",
                "this",
                "that",
                "it",
                "new",
                "old",
                "my",
                "our",
                "their",
                "its",
                "some",
                "any",
                "no",
                "added",
                "removed",
                "deleted",
                "renamed",
                "created",
                "dropped",
                "wired",
                "made",
                "is",
                "was",
                "exists",
                "exist",
                "ex",
                "still",
                "present",
                "called",
                "named", // prepositions: "function IN refs.rs" must not
                // yield `in` as the symbol name — the real name is name-first ("X function").
                "in",
                "to",
                "from",
                "at",
                "on",
                "of",
                "into",
                "with",
                "for",
                // auxiliary verbs: "the X method HAS BEEN added" must not yield
                // `has` as the symbol — "method has" matched kind-first (wild
                // false-contradiction caught by truth on itself, 2026-06-13).
                "has",
                "have",
                "had",
                "been",
                "being",
                "be",
                "will",
                "would",
                "can",
                "could",
                "should",
                "successfully",
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
            //
            // BOTH paths require the name to LOOK like an identifier. Kind-first
            // "the field_names method NOW returns" / "method ARE working" used to
            // grab the trailing English word (`now`/`are`) as the symbol — a
            // whole class of false verdicts the SWE-bench calibration surfaced.
            // A real symbol is snake_case / camelCase / has a digit / is
            // backticked; plain lowercase prose words never are.
            let name = r
                .symbol
                .captures(text)
                .and_then(|c| c.get(1))
                .filter(identifier_like)
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
            || lower.contains("wired")
            // A handler/server for a literal /path is evidence the route exists:
            // "we handle POST /v1/checkout", "serves GET /x".
            || lower.contains("handle")
            || lower.contains("serve");
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

/// Parse a small count from a digit token ("5") or a spelled-out number
/// ("five"). Spelled numbers only cover one..twelve — enough for the retry/
/// timeout phrasings agents use ("retried five times"); larger values are
/// effectively always written as digits.
fn parse_count(token: &str) -> Option<i64> {
    if let Ok(n) = token.parse::<i64>() {
        return Some(n);
    }
    let n = match token.trim().to_ascii_lowercase().as_str() {
        "one" => 1,
        "two" => 2,
        "three" => 3,
        "four" => 4,
        "five" => 5,
        "six" => 6,
        "seven" => 7,
        "eight" => 8,
        "nine" => 9,
        "ten" => 10,
        "eleven" => 11,
        "twelve" => 12,
        _ => return None,
    };
    Some(n)
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
/// Whether the sentence carries an explicit symbol-KIND phrase
/// ("X function", "the Y struct", "Z field") — in which case a co-mentioned
/// filename is a location, not the subject, and the symbol claim takes priority.
fn has_symbol_kind_phrase(text: &str) -> bool {
    res().symbol.is_match(text) || res().symbol_post.is_match(text)
}

/// Whether the sentence carries an "import" cue ("added an os import"). An added
/// import is a dependency-use claim, so a co-mentioned filename is a location,
/// not a file_changed subject. `lower` is expected lowercase.
fn has_import_phrase(lower: &str) -> bool {
    has_word(lower, "import") || has_word(lower, "imports") || has_word(lower, "imported")
}

/// Whole-word containment: `word` appears in `haystack` bounded by non-alnum
/// (and non-`_`/`-`) on both sides. So "dependency" matches in "a dependency
/// file" but NOT inside the identifier "dependency_index_populated". `haystack`
/// is expected lowercase; `word` must be lowercase.
fn has_word(haystack: &str, word: &str) -> bool {
    let is_boundary = |c: Option<char>| match c {
        None => true,
        Some(c) => !(c.is_ascii_alphanumeric() || c == '_' || c == '-'),
    };
    let bytes = haystack.as_bytes();
    let mut from = 0;
    while let Some(rel) = haystack[from..].find(word) {
        let start = from + rel;
        let end = start + word.len();
        let before = haystack[..start].chars().next_back();
        let after = if end < bytes.len() {
            haystack[end..].chars().next()
        } else {
            None
        };
        if is_boundary(before) && is_boundary(after) {
            return true;
        }
        from = start + 1;
    }
    false
}

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
    // Cue words after which the next non-stopword token is the package. Most
    // specific cues first; the generic "the" is tried LAST so phrasings like
    // "depends on the tokio crate" resolve on `on`/`the-after-on`, not on the
    // leading "the project" (which once yielded the preposition `on`).
    const AFTER: &[&str] = &["uses", "use", "using", "on", "upon", "added", "adds", "the"];
    // Words that are never package names in this position. Includes the cue
    // verbs/prepositions themselves so "the project depends on serde" returns
    // `serde`, never `on`/`depends`/`uses`.
    const STOP: &[&str] = &[
        "a",
        "an",
        "the",
        "as",
        "to",
        "of",
        "on",
        "upon",
        "and",
        // Connectives/prepositions that sit before the dep noun in pure prose —
        // "a workspace WITH crates", "code FOR packages", "split INTO crates".
        // Without these the BEFORE-cue loop took the function word right before
        // "crate(s)/package" as the package name: "a Rust workspace with crates/"
        // mined `with` and contradicted a true sentence (caught on truth itself
        // 2026-06-23).
        "with",
        "for",
        "from",
        "by",
        "in",
        "into",
        "via",
        "at",
        "or",
        "but",
        "it",
        "is",
        "was",
        "has",
        "have",
        "this",
        "that",
        "dependency",
        "dependencies",
        "crate",
        "crates",
        "package",
        "packages",
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
        "libraries",
        "lib",
        // verbs/adverbs that follow "depend on it" prose — never packages.
        // "the headline doesn't depend on it — it needs only ..." once mined
        // `needs` as a package (caught by truth on itself, 2026-06-13).
        "needs",
        "need",
        "needed",
        "requires",
        "require",
        "only",
        "just",
        "now",
        "still",
        "really",
        // "import" is a dependency CUE word, not a package name; "cue"/"cues"
        // are common nouns. "added import as a dependency cue" once mined
        // `import`/`cue` as the package and contradicted a true sentence
        // (verify-turn over a summary, caught on truth itself 2026-06-19).
        "import",
        "imports",
        "imported",
        "cue",
        "cues",
    ];
    let plausible = |cand: &str| -> bool {
        cand.len() >= 2
            && cand.chars().next().is_some_and(|c| c.is_ascii_lowercase())
            && cand
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    };
    // Registry names almost always carry a `-`/`_`/digit, or are backticked in
    // the prose. Plain lowercase English words ("real" in "a real library",
    // "standard library", "a clever toy") do NOT — and matching them produced
    // false-contradicted dependency claims out of rhetoric (caught live
    // 2026-06-12). The BEFORE-cue path demands this shape; the explicit-verb
    // AFTER-cue path ("uses the serde crate") may take a bare name.
    let package_shaped = |cand: &str| -> bool {
        plausible(cand)
            && (cand.contains('-')
                || cand.contains('_')
                || cand.chars().any(|c| c.is_ascii_digit())
                || text.contains(&format!("`{cand}`")))
    };

    let tokens: Vec<&str> = text
        .split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-')
        .filter(|t| !t.is_empty())
        .collect();
    let lower_tokens: Vec<String> = tokens.iter().map(|t| t.to_ascii_lowercase()).collect();

    // Package-BEFORE-cue. Two tiers by how unambiguous the noun is:
    //  STRONG ("dependency"/"crate"/"package") — these words are rarely prose,
    //    so a plain plausible name before them is the package ("jest dependency",
    //    "express package", "the serde crate").
    //  WEAK ("library") — collides with prose ("standard library", "real
    //    library"), so it only counts when the name is registry-SHAPED.
    const STRONG_NOUN: &[&str] = &["dependency", "dependencies", "crate", "crates", "package"];
    const WEAK_NOUN: &[&str] = &["library"];
    for i in 1..lower_tokens.len() {
        let prev = &lower_tokens[i - 1];
        if STOP.contains(&prev.as_str()) {
            continue;
        }
        let cur = lower_tokens[i].as_str();
        if STRONG_NOUN.contains(&cur) && plausible(prev) {
            return Some(prev.clone());
        }
        if WEAK_NOUN.contains(&cur) && package_shaped(prev) {
            return Some(prev.clone());
        }
    }

    // A bare, non-package-shaped name (e.g. `serde`, `tokio`) may only be taken
    // when the sentence carries an explicit dependency-relationship marker —
    // "depends on serde", "the serde crate/dependency". Without one, the weak
    // cues (`the`/`on`/`use`) are pure-prose collisions ("use a standard
    // library", "the real thing") and must NOT yield a package.
    let strong_dep_marker = lower.contains("depends on")
        || lower.contains("depend on")
        || has_word(lower, "dependency")
        || has_word(lower, "dependencies")
        || has_word(lower, "crate")
        || has_word(lower, "crates")
        || has_word(lower, "package");

    // Package-AFTER-cue, most specific cue first. The generic `the` cue is too
    // weak to license a bare prose word even with a dep marker present — "the
    // eval ground truth" in "...doesn't depend on it, it needs only the eval..."
    // mined `eval` (caught by truth on itself). After `the`, demand a package
    // SHAPE; only the verb/preposition cues may take a bare name.
    for cue in AFTER {
        if let Some(i) = lower_tokens.iter().position(|t| t == cue) {
            let weak_cue = *cue == "the";
            // The package sits RIGHT AFTER the cue (modulo one article/adjective):
            // "depends on serde", "on the serde crate". Scanning past many
            // stopwords grabbed a word from a later clause — "...depend on it, it
            // needs only the eval..." mined `eval`, 6 tokens downstream across a
            // comma. Allow skipping at most two stopwords, then give up.
            let mut skipped = 0;
            for cand in lower_tokens.iter().skip(i + 1) {
                if STOP.contains(&cand.as_str()) {
                    skipped += 1;
                    if skipped > 2 {
                        break;
                    }
                    continue;
                }
                let take_bare = strong_dep_marker && plausible(cand) && !weak_cue;
                if package_shaped(cand) || take_bare {
                    return Some(cand.clone());
                }
                break;
            }
        }
    }
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
    fn natural_recall_phrasings() {
        // Recall wins added 2026-06-12 — natural agent phrasings that used to
        // refuse. Each must extract the right type and value.
        let cases: &[(&str, ClaimType, f64)] = &[
            // spelled-out retry count
            (
                "payments are retried five times before giving up",
                ClaimType::RetryCount,
                5.0,
            ),
            // "up to N times" with a long cue→count gap
            (
                "we attempt the payment up to 5 times",
                ClaimType::RetryCount,
                5.0,
            ),
            // "time out after Ns" — two-word + trailing unit
            ("requests time out after 30s", ClaimType::TimeoutValue, 30.0),
            // "listening on N" — port without the word "port"
            (
                "the server is listening on 8080",
                ClaimType::ConfigValue,
                8080.0,
            ),
        ];
        for (text, ty, val) in cases {
            let c = RegexExtractor.extract(text);
            assert_eq!(c.claim_type, *ty, "{text}");
            assert_eq!(c.expected_number(), Some(*val), "{text}");
        }
        // route handler phrasing → a route-existence claim on the literal path
        let r = RegexExtractor.extract("we handle POST /v1/checkout");
        assert_eq!(r.claim_type, ClaimType::RouteExists);
        assert_eq!(r.subject.as_deref(), Some("/v1/checkout"));
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
    fn dependency_recall_strong_nouns_take_bare_names() {
        // Strong dependency nouns (dependency/crate/package) license a plain
        // package name; only `library` needs a registry-shaped name. These are
        // the corpus D-band phrasings.
        for (text, want) in [
            ("the project uses the express package", "express"),
            ("it has a jest dependency", "jest"),
            ("we depend on stripe", "stripe"),
        ] {
            let c = RegexExtractor.extract(text);
            assert_eq!(c.claim_type, ClaimType::DependencyUsed, "{text}");
            assert_eq!(c.subject.as_deref(), Some(want), "{text}");
        }
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

        // Regression: "depends on the X crate" once extracted the preposition
        // `on` (the leading "the project" matched the generic cue, then the next
        // non-stopword was `on`). Must yield the package.
        let e = RegexExtractor.extract("the project depends on the tree-sitter crate");
        assert_eq!(e.claim_type, ClaimType::DependencyUsed);
        assert_eq!(e.subject.as_deref(), Some("tree-sitter"));

        // Regression: package BEFORE the noun cue ("X dependency") returned no
        // subject at all (forward-only scan) → a true claim went Inconclusive.
        let f = RegexExtractor.extract("truth-ast has a tree-sitter-typescript dependency");
        assert_eq!(f.claim_type, ClaimType::DependencyUsed);
        assert_eq!(f.subject.as_deref(), Some("tree-sitter-typescript"));
    }

    #[test]
    fn prose_describing_workspace_is_not_a_dependency_claim() {
        // Caught on truth itself 2026-06-23: verify-turn over a true sentence,
        // "this is the truth project, a Rust workspace with crates/". The
        // BEFORE-cue loop saw `crates` (a STRONG_NOUN) and took the immediately
        // preceding function word `with` as the package name, then the verify
        // engine "contradicted" it on the omnipresent substring count. A
        // connective sitting before the dep noun is never a package — the claim
        // must not extract a dependency subject at all.
        for text in [
            "a Rust workspace with crates/",
            "this is the truth project, a Rust workspace with crates/",
            "we split the code into crates",
            "the build is organized by packages",
        ] {
            let c = RegexExtractor.extract(text);
            assert_ne!(
                c.subject.as_deref(),
                Some("with"),
                "must not mine the connective as a package: {text}"
            );
            // None of these connectives are valid package names.
            for stop in ["with", "into", "by", "for", "from"] {
                assert_ne!(
                    c.subject.as_deref(),
                    Some(stop),
                    "extracted connective `{stop}` as a dependency from: {text}"
                );
            }
        }
    }

    #[test]
    fn prose_with_library_word_is_not_a_dependency_claim() {
        // Caught live 2026-06-12: "a real library or a clever toy" fuzzy-matched
        // a dependency claim (subject `real`/`standard`) and got Contradicted —
        // rhetoric judged as a missing dep. Plain English around library/use/the
        // must not yield a package.
        for text in [
            "the one that decides whether this is a real library or a clever toy",
            "this is a real library or a clever toy",
            "we should use a standard library function",
            "the real thing is the parser",
        ] {
            let c = RegexExtractor.extract(text);
            assert_ne!(c.claim_type, ClaimType::DependencyUsed, "{text}");
        }
    }

    #[test]
    fn identifier_containing_dependency_is_not_a_dep_claim() {
        // "added a dependency_index_populated field" — the identifier CONTAINS
        // "dependency" but the claim is about a symbol, not a dependency. Whole-
        // word cue matching prevents the mis-type.
        let c =
            RegexExtractor.extract("I added a dependency_index_populated field to VerdictInput");
        assert_ne!(c.claim_type, ClaimType::DependencyUsed);
    }

    #[test]
    fn aux_verb_after_kind_is_not_the_symbol_name() {
        // "the get_view_plugins method has been added" — kind-first matched
        // "method has" and yielded `has` as the symbol (truth flagged this on
        // its own summary, 2026-06-13). The name is the identifier BEFORE the
        // kind, never the auxiliary verb after it.
        let c = RegexExtractor.extract("The get_view_plugins method has been successfully added");
        assert_eq!(c.claim_type, ClaimType::SymbolExists);
        assert_eq!(c.subject.as_deref(), Some("get_view_plugins"));
    }

    #[test]
    fn kind_first_prose_word_is_not_a_symbol() {
        // "the field_names method NOW returns" / "method ARE working" grabbed the
        // trailing English word (`now`/`are`) kind-first as the symbol — a whole
        // false-verdict class the SWE-bench calibration surfaced. A symbol must
        // look like an identifier; plain prose words after a kind word do not.
        for text in [
            "the field_names method now returns a view of the keys",
            "the handleNodeLoad method are working as expected",
            "the parser function still behaves correctly",
        ] {
            let c = RegexExtractor.extract(text);
            // Either not a symbol claim, or it resolved the REAL identifier —
            // never the bare prose word.
            if c.claim_type == ClaimType::SymbolExists {
                let s = c.subject.as_deref().unwrap_or("");
                assert!(
                    !["now", "are", "still", "has", "be"].contains(&s),
                    "grabbed prose word `{s}` from: {text}"
                );
            }
        }
    }

    #[test]
    fn depend_on_pronoun_does_not_mine_a_downstream_word() {
        // "...doesn't depend on it, it needs only the eval ground truth" mined
        // `eval` (6 tokens past the cue, across a comma) as a dependency. The
        // package sits right after the cue, not in a later clause — refuse.
        let c = RegexExtractor
            .extract("The 30% headline does not depend on it, it needs only the eval ground truth");
        assert_ne!(c.claim_type, ClaimType::DependencyUsed, "{:?}", c.subject);
    }

    #[test]
    fn symbol_in_file_is_a_symbol_claim_not_file_claim() {
        // "added a X function in refs.rs" — the file is a LOCATION; the assertion
        // is about the symbol. Must be a SymbolExists claim on the symbol, not a
        // FileChanged claim on `refs.rs` (which contradicted when the file wasn't
        // in the current diff — a residual FP `stats --review` surfaced).
        let c = RegexExtractor.extract("I added a dependency_index_populated function in refs.rs");
        assert_eq!(c.claim_type, ClaimType::SymbolExists);
        assert_eq!(c.subject.as_deref(), Some("dependency_index_populated"));
    }

    #[test]
    fn added_import_is_a_dependency_claim_not_file_claim() {
        // "added an os import to fetch.py" — the import is the assertion; the
        // file is a LOCATION. Must NOT be a FileChanged/added claim on fetch.py
        // (which CONTRADICTED when fetch.py pre-existed: "claimed added, but the
        // diff shows fetch.py was modified") — an FP the SWE-bench calibration
        // loop surfaced on truth's own working tree.
        let c = RegexExtractor.extract("I added an os import to fetch.py");
        assert_ne!(
            c.claim_type,
            ClaimType::FileChanged,
            "import claim must not type as file_changed: {:?}",
            c.subject
        );
    }

    #[test]
    fn bare_import_cue_word_is_not_a_dependency_subject() {
        // "added import as a dependency cue" — meta-prose about an extractor
        // cue, not a real dependency claim. `import`/`cue` must NOT be mined as
        // the package (it contradicted a true sentence when verify-turn ran over
        // a summary). Either not a dep claim, or no bogus subject.
        let c = RegexExtractor.extract("I added import as a dependency cue in extract.rs");
        if c.claim_type == ClaimType::DependencyUsed {
            let subj = c.subject.as_deref().unwrap_or("");
            assert!(
                subj != "import" && subj != "cue" && subj != "imports",
                "bare cue word mined as package: {subj:?}"
            );
        }
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
