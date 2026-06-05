//! Safe query templates (spec §11.2) and the evidence-query spec/result types
//! exchanged with queryable sources (spec §12.2). The LLM may only emit these
//! templates — never raw LogQL/SQL.

use crate::enums::SourceKind;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Allowed `query_type` values for v0.1 (spec §11.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryType {
    RouteCount,
    ErrorCount,
    LatestOccurrence,
    JobSuccess,
    EventCount,
    RouteExists,
    ConfigValue,
    EnvVarExists,
    DependencyExists,
    ConstantValue,
}

impl QueryType {
    /// Stable snake_case label used in `EvidenceQuery.query_type`.
    pub fn as_label(&self) -> &'static str {
        match self {
            QueryType::RouteCount => "route_count",
            QueryType::ErrorCount => "error_count",
            QueryType::LatestOccurrence => "latest_occurrence",
            QueryType::JobSuccess => "job_success",
            QueryType::EventCount => "event_count",
            QueryType::RouteExists => "route_exists",
            QueryType::ConfigValue => "config_value",
            QueryType::EnvVarExists => "env_var_exists",
            QueryType::DependencyExists => "dependency_exists",
            QueryType::ConstantValue => "constant_value",
        }
    }
}

/// A single planned query. Field set is the union over all query types; only
/// the relevant fields are populated for a given `query_type`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannedQuery {
    pub source: SourceKind,
    pub query_type: QueryType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
}

/// A full query plan emitted by the planner.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QueryPlan {
    pub queries: Vec<PlannedQuery>,
}

/// Spec passed to a `QueryableSource` to execute one query.
#[derive(Debug, Clone)]
pub struct EvidenceQuerySpec {
    pub query_type: QueryType,
    /// Primary search term (route or pattern), if any.
    pub needle: Option<String>,
    pub window: Option<String>,
    pub environment: Option<String>,
    pub service: Option<String>,
}

/// Normalized result returned by a `QueryableSource`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidenceQueryResult {
    pub source: SourceKind,
    pub query_type: QueryType,
    /// The provider-native query text actually executed (e.g. LogQL).
    pub query_text: String,
    /// Aggregate count, when meaningful for the query type.
    pub count: Option<i64>,
    /// Latest matching occurrence, as a unix epoch second.
    pub latest_seen: Option<i64>,
    /// Up to N redacted sample lines (never raw logs beyond this).
    pub redacted_samples: Vec<String>,
    pub time_from: Option<i64>,
    pub time_to: Option<i64>,
    /// Anything extra worth persisting in the summary blob.
    pub extra: Value,
}

impl EvidenceQueryResult {
    /// Build the `result_summary_json` blob stored on `EvidenceQuery`.
    pub fn summary_json(&self) -> Value {
        serde_json::json!({
            "count": self.count,
            "latest_seen": self.latest_seen,
            "redacted_samples": self.redacted_samples,
            "extra": self.extra,
        })
    }
}
