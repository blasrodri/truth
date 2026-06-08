//! Local log file adapter — implements the same query templates as Loki over a
//! plain text **or** JSON-lines log file, so demos run with no Loki container.
//!
//! Plain-text lines may begin with an RFC3339 timestamp (used for `latest_seen`).
//! JSON-lines entries are parsed structurally: route/path fields are matched for
//! route/event queries, and error/message fields for error queries.

use crate::redact::redact_line;
use crate::window::{clamp_window_secs, window_secs};
use anyhow::{Context, Result};
use truth_core::enums::SourceKind;
use truth_core::query::{EvidenceQueryResult, EvidenceQuerySpec, QueryType};
use truth_core::traits::QueryableSource;

pub struct LocalFileSource {
    path: String,
    max_window_days: u32,
    max_samples: usize,
    include_samples: bool,
}

impl LocalFileSource {
    pub fn new(
        path: impl Into<String>,
        max_window_days: u32,
        max_samples: usize,
        include_samples: bool,
    ) -> Self {
        Self {
            path: path.into(),
            max_window_days,
            max_samples,
            include_samples,
        }
    }

    fn needle<'a>(&self, spec: &'a EvidenceQuerySpec) -> &'a str {
        spec.needle.as_deref().unwrap_or("")
    }
}

/// Best-effort: parse a leading ISO/RFC3339 timestamp from a plain-text line.
fn parse_leading_ts(line: &str) -> Option<i64> {
    let token = line.split_whitespace().next()?;
    chrono::DateTime::parse_from_rfc3339(token)
        .ok()
        .map(|dt| dt.timestamp())
}

/// Parse a timestamp from common JSON timestamp fields.
fn parse_json_ts(v: &serde_json::Value) -> Option<i64> {
    for key in ["timestamp", "time", "ts", "@timestamp"] {
        if let Some(s) = v.get(key).and_then(|x| x.as_str()) {
            if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
                return Some(dt.timestamp());
            }
        }
        if let Some(n) = v.get(key).and_then(|x| x.as_i64()) {
            // Heuristic: ms epoch if it's clearly too large for seconds.
            return Some(if n > 100_000_000_000 { n / 1000 } else { n });
        }
    }
    None
}

/// Whether a single log entry matches the query.
enum Match {
    No,
    Yes { ts: Option<i64>, sample: String },
}

impl LocalFileSource {
    fn match_line(&self, line: &str, needle: &str, want_error: bool) -> Match {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return Match::No;
        }

        // Try JSON-lines first.
        if trimmed.starts_with('{') {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
                return self.match_json(&v, line, needle, want_error);
            }
        }

        // Plain text fallback.
        let needle_ok = needle.is_empty() || line.contains(needle);
        if !needle_ok {
            return Match::No;
        }
        if want_error && !looks_like_error(line) {
            return Match::No;
        }
        Match::Yes {
            ts: parse_leading_ts(line),
            sample: redact_line(line),
        }
    }

    fn match_json(
        &self,
        v: &serde_json::Value,
        raw: &str,
        needle: &str,
        want_error: bool,
    ) -> Match {
        let field = |k: &str| v.get(k).and_then(|x| x.as_str());

        // Fields searched for route/event vs error queries.
        let route_fields = ["route", "path", "url", "uri"];
        let error_fields = ["error", "err", "message", "msg", "exception"];

        let contains_needle = |fields: &[&str]| -> bool {
            if needle.is_empty() {
                return true;
            }
            fields
                .iter()
                .filter_map(|k| field(k))
                .any(|val| val.contains(needle))
        };

        let level_is_error = field("level")
            .map(|l| {
                let l = l.to_ascii_lowercase();
                l == "error" || l == "fatal" || l == "critical"
            })
            .unwrap_or(false);
        let status_is_error = v
            .get("status")
            .and_then(|s| s.as_i64())
            .map(|s| s >= 500)
            .unwrap_or(false);

        let matched = if want_error {
            if needle.is_empty() {
                // No pattern: any error-level/5xx entry counts.
                level_is_error || status_is_error
            } else {
                // Pattern given: it must appear in an error/message field.
                contains_needle(&error_fields)
            }
        } else {
            contains_needle(&route_fields)
        };

        if !matched {
            return Match::No;
        }
        Match::Yes {
            ts: parse_json_ts(v),
            sample: redact_line(raw),
        }
    }
}

impl QueryableSource for LocalFileSource {
    fn execute_query(&self, spec: EvidenceQuerySpec) -> Result<EvidenceQueryResult> {
        let contents = std::fs::read_to_string(&self.path)
            .with_context(|| format!("reading local log file {}", self.path))?;
        let needle = self.needle(&spec);
        let want_error = matches!(spec.query_type, QueryType::ErrorCount);

        let mut count = 0i64;
        let mut latest: Option<i64> = None;
        let mut samples: Vec<String> = Vec::new();

        for line in contents.lines() {
            if let Match::Yes { ts, sample } = self.match_line(line, needle, want_error) {
                count += 1;
                if let Some(t) = ts {
                    latest = Some(latest.map_or(t, |l| l.max(t)));
                }
                if self.include_samples && samples.len() < self.max_samples {
                    samples.push(sample);
                }
            }
        }

        let window = clamp_window_secs(window_secs(spec.window.as_deref()), self.max_window_days);
        let now = truth_core::now_secs();

        Ok(EvidenceQueryResult {
            source: SourceKind::LocalLogs,
            query_type: spec.query_type,
            query_text: format!("localfile match {:?} in {}", needle, self.path),
            count: Some(count),
            latest_seen: latest,
            redacted_samples: samples,
            time_from: Some(now - window),
            time_to: Some(now),
            extra: serde_json::json!({ "file": self.path }),
        })
    }
}

fn looks_like_error(line: &str) -> bool {
    let l = line.to_ascii_lowercase();
    l.contains(" 500 ")
        || l.contains(" 502 ")
        || l.contains(" 503 ")
        || l.contains("error")
        || l.contains("timeout")
        || l.contains("exception")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmp_log(name: &str, contents: &str) -> String {
        let path = std::env::temp_dir().join(format!("truth_test_{name}.log"));
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        path.to_string_lossy().into_owned()
    }

    fn spec(qt: QueryType, needle: &str) -> EvidenceQuerySpec {
        EvidenceQuerySpec {
            query_type: qt,
            needle: Some(needle.into()),
            window: Some("7d".into()),
            environment: None,
            service: None,
        }
    }

    #[test]
    fn counts_route_matches_and_redacts_samples() {
        let log = "2026-06-04T14:03:00Z GET /v1/checkout 200 user bob@x.com\n\
                   2026-06-04T14:04:00Z POST /v1/checkout 500 timeout\n\
                   2026-06-04T14:05:00Z GET /health 200\n";
        let src = LocalFileSource::new(tmp_log("plain", log), 30, 3, true);
        let r = src
            .execute_query(spec(QueryType::RouteCount, "/v1/checkout"))
            .unwrap();
        assert_eq!(r.count, Some(2));
        assert!(r.latest_seen.is_some());
        assert!(r
            .redacted_samples
            .iter()
            .any(|s| s.contains("[REDACTED_EMAIL]")));
    }

    #[test]
    fn error_count_filters_errors_plaintext() {
        let log = "GET /v1/checkout 200\nPOST /v1/checkout 500 timeout\n";
        let src = LocalFileSource::new(tmp_log("plainerr", log), 30, 3, true);
        let r = src
            .execute_query(spec(QueryType::ErrorCount, "/v1/checkout"))
            .unwrap();
        assert_eq!(r.count, Some(1));
    }

    #[test]
    fn jsonl_route_count_matches_route_field() {
        let log = r#"{"timestamp":"2026-06-04T14:03:00Z","level":"info","route":"/v1/checkout","status":200}
{"timestamp":"2026-06-04T14:04:00Z","level":"error","route":"/v1/checkout","error":"payment_timeout","user_email":"x@example.com"}
{"timestamp":"2026-06-04T14:05:00Z","route":"/health","status":200}
"#;
        let src = LocalFileSource::new(tmp_log("jsonl", log), 30, 3, true);
        let r = src
            .execute_query(spec(QueryType::RouteCount, "/v1/checkout"))
            .unwrap();
        assert_eq!(r.count, Some(2));
        assert_eq!(
            r.latest_seen,
            Some(
                chrono::DateTime::parse_from_rfc3339("2026-06-04T14:04:00Z")
                    .unwrap()
                    .timestamp()
            )
        );
        // Email inside the JSON is redacted in the stored sample.
        assert!(r
            .redacted_samples
            .iter()
            .any(|s| s.contains("[REDACTED_EMAIL]")));
    }

    #[test]
    fn jsonl_error_count_matches_error_and_message() {
        let log = r#"{"timestamp":"2026-06-04T14:04:00Z","level":"error","error":"payment_timeout"}
{"timestamp":"2026-06-04T14:06:00Z","level":"error","message":"payment_timeout while charging"}
{"timestamp":"2026-06-04T14:07:00Z","level":"info","route":"/v1/checkout"}
"#;
        let src = LocalFileSource::new(tmp_log("jsonlerr", log), 30, 3, true);
        let r = src
            .execute_query(spec(QueryType::ErrorCount, "payment_timeout"))
            .unwrap();
        assert_eq!(r.count, Some(2));
    }

    #[test]
    fn jsonl_latest_occurrence() {
        let log = r#"{"timestamp":"2026-06-04T14:03:00Z","route":"/v1/checkout"}
{"timestamp":"2026-06-04T15:03:00Z","route":"/v1/checkout"}
"#;
        let src = LocalFileSource::new(tmp_log("jsonllatest", log), 30, 3, true);
        let r = src
            .execute_query(spec(QueryType::LatestOccurrence, "/v1/checkout"))
            .unwrap();
        assert_eq!(
            r.latest_seen,
            Some(
                chrono::DateTime::parse_from_rfc3339("2026-06-04T15:03:00Z")
                    .unwrap()
                    .timestamp()
            )
        );
    }
}
