//! Loki adapter. The adapter — not the LLM — constructs LogQL from safe
//! templates (spec §12.3, §15.3).

use crate::redact::redact_line;
use crate::window::{clamp_window_secs, window_secs};
use anyhow::{Context, Result};
use std::collections::HashMap;
use truth_core::enums::SourceKind;
use truth_core::query::{EvidenceQueryResult, EvidenceQuerySpec, QueryType};
use truth_core::traits::QueryableSource;

pub struct LokiSource {
    base_url: String,
    default_env: String,
    /// Label-name remapping (config key -> actual Loki label). e.g. env -> env.
    labels: HashMap<String, String>,
    max_window_days: u32,
    max_samples: usize,
    include_samples: bool,
}

impl LokiSource {
    pub fn new(
        base_url: impl Into<String>,
        default_env: impl Into<String>,
        labels: HashMap<String, String>,
        max_window_days: u32,
        max_samples: usize,
        include_samples: bool,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            default_env: default_env.into(),
            labels,
            max_window_days,
            max_samples,
            include_samples,
        }
    }

    fn label(&self, key: &str) -> String {
        self.labels
            .get(key)
            .cloned()
            .unwrap_or_else(|| key.to_string())
    }

    /// Build the label selector `{env="prod", service="api"}`.
    fn selector(&self, spec: &EvidenceQuerySpec) -> String {
        let env = spec.environment.as_deref().unwrap_or(&self.default_env);
        let mut parts = vec![format!("{}=\"{}\"", self.label("env"), escape(env))];
        if let Some(svc) = &spec.service {
            parts.push(format!("{}=\"{}\"", self.label("service"), escape(svc)));
        }
        format!("{{{}}}", parts.join(", "))
    }

    /// Construct the LogQL for a query spec.
    pub fn build_logql(&self, spec: &EvidenceQuerySpec, window: &str) -> String {
        let selector = self.selector(spec);
        let needle = spec.needle.as_deref().unwrap_or("");
        match spec.query_type {
            QueryType::RouteCount | QueryType::EventCount => format!(
                "sum(count_over_time({selector} |= \"{}\" [{window}]))",
                escape(needle)
            ),
            QueryType::ErrorCount => format!(
                "sum(count_over_time({selector} |= \"{}\" |~ \"(?i)(error|timeout|5\\\\d\\\\d)\" [{window}]))",
                escape(needle)
            ),
            QueryType::LatestOccurrence => {
                format!("{selector} |= \"{}\"", escape(needle))
            }
            QueryType::JobSuccess => format!(
                "sum(count_over_time({selector} |= \"{}\" |= \"success\" [{window}]))",
                escape(needle)
            ),
            // Non-log query types should not reach the Loki adapter.
            _ => format!("{selector} |= \"{}\"", escape(needle)),
        }
    }
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

impl QueryableSource for LokiSource {
    fn execute_query(&self, spec: EvidenceQuerySpec) -> Result<EvidenceQueryResult> {
        let window_s = clamp_window_secs(window_secs(spec.window.as_deref()), self.max_window_days);
        let window = format!("{}s", window_s);
        let logql = self.build_logql(&spec, &window);

        let now = truth_core::now_secs();
        let from = now - window_s;

        let is_instant = matches!(
            spec.query_type,
            QueryType::RouteCount
                | QueryType::EventCount
                | QueryType::ErrorCount
                | QueryType::JobSuccess
        );

        let (count, latest, samples) = if is_instant {
            let count = self
                .query_instant(&logql, now)
                .context("loki instant query")?;
            (Some(count), None, vec![])
        } else {
            let (latest, samples) = self
                .query_range(&logql, from, now)
                .context("loki range query")?;
            (None, latest, samples)
        };

        Ok(EvidenceQueryResult {
            source: SourceKind::Loki,
            query_type: spec.query_type,
            query_text: logql,
            count,
            latest_seen: latest,
            redacted_samples: samples,
            time_from: Some(from),
            time_to: Some(now),
            extra: serde_json::json!({}),
        })
    }
}

impl LokiSource {
    fn query_instant(&self, logql: &str, time_secs: i64) -> Result<i64> {
        let url = format!("{}/loki/api/v1/query", self.base_url);
        let resp = ureq::get(&url)
            .query("query", logql)
            .query("time", &format!("{}000000000", time_secs))
            .call()?;
        let v: serde_json::Value = resp.into_json()?;
        // data.result[].value = [ts, "count"]
        let total = v["data"]["result"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|r| r["value"][1].as_str())
                    .filter_map(|s| s.parse::<f64>().ok())
                    .sum::<f64>()
            })
            .unwrap_or(0.0);
        Ok(total as i64)
    }

    fn query_range(&self, logql: &str, from: i64, to: i64) -> Result<(Option<i64>, Vec<String>)> {
        let url = format!("{}/loki/api/v1/query_range", self.base_url);
        let resp = ureq::get(&url)
            .query("query", logql)
            .query("start", &format!("{}000000000", from))
            .query("end", &format!("{}000000000", to))
            .query("direction", "backward")
            .query("limit", &self.max_samples.max(1).to_string())
            .call()?;
        let v: serde_json::Value = resp.into_json()?;

        let mut latest: Option<i64> = None;
        let mut samples = Vec::new();
        if let Some(streams) = v["data"]["result"].as_array() {
            for stream in streams {
                if let Some(entries) = stream["values"].as_array() {
                    for entry in entries {
                        if let Some(ts_ns) = entry[0].as_str().and_then(|s| s.parse::<i64>().ok()) {
                            let ts = ts_ns / 1_000_000_000;
                            latest = Some(latest.map_or(ts, |l| l.max(ts)));
                        }
                        if self.include_samples && samples.len() < self.max_samples {
                            if let Some(line) = entry[1].as_str() {
                                samples.push(redact_line(line));
                            }
                        }
                    }
                }
            }
        }
        Ok((latest, samples))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src() -> LokiSource {
        let mut labels = HashMap::new();
        labels.insert("env".into(), "env".into());
        labels.insert("service".into(), "service".into());
        LokiSource::new("http://x", "prod", labels, 30, 3, true)
    }

    #[test]
    fn route_count_logql_matches_spec_shape() {
        let spec = EvidenceQuerySpec {
            query_type: QueryType::RouteCount,
            needle: Some("/v1/checkout".into()),
            window: Some("7d".into()),
            environment: Some("prod".into()),
            service: Some("api".into()),
        };
        let q = src().build_logql(&spec, "7d");
        assert_eq!(
            q,
            "sum(count_over_time({env=\"prod\", service=\"api\"} |= \"/v1/checkout\" [7d]))"
        );
    }

    #[test]
    fn escapes_quotes_in_needle() {
        let spec = EvidenceQuerySpec {
            query_type: QueryType::RouteCount,
            needle: Some("a\"b".into()),
            window: None,
            environment: None,
            service: None,
        };
        let q = src().build_logql(&spec, "7d");
        assert!(q.contains("a\\\"b"));
    }
}
