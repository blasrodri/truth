//! Domain models, ported from the product spec §8. Timestamps are unix epoch
//! seconds (`i64`) to match the SQLite schema. IDs are `String` (UUIDv4).

use crate::enums::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Anything that can be cited.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Artifact {
    pub id: String,
    pub source: SourceKind,
    pub kind: ArtifactKind,
    pub uri: String,
    pub external_id: Option<String>,
    pub hash: Option<String>,
    pub authored_at: Option<i64>,
    pub observed_at: i64,
    pub author: Option<String>,
    pub metadata_json: Value,
}

/// A precise location inside an artifact.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Span {
    pub id: String,
    pub artifact_id: String,
    pub text: String,
    pub start_line: Option<u32>,
    pub end_line: Option<u32>,
    pub start_byte: Option<u32>,
    pub end_byte: Option<u32>,
    pub metadata_json: Value,
}

/// Normalizes aliases for a real-world thing (endpoint, dependency, ...).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Concept {
    pub id: String,
    pub canonical_name: String,
    pub kind: Option<String>,
    pub aliases_json: Value,
    pub metadata_json: Value,
}

/// The central normalized evidence object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidenceItem {
    pub id: String,
    pub span_id: String,
    pub evidence_type: EvidenceType,
    pub subject_text: Option<String>,
    pub subject_concept_id: Option<String>,
    pub predicate: Option<String>,
    pub object_text: Option<String>,
    pub value_json: Option<Value>,
    pub unit: Option<String>,
    pub confidence: f32,
    pub authority: Authority,
    pub valid_from: Option<i64>,
    pub valid_to: Option<i64>,
    pub extraction_method: ExtractionMethod,
    pub metadata_json: Value,
}

/// One verification job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Check {
    pub id: String,
    pub trigger: Trigger,
    pub question: String,
    pub question_type: Option<QuestionType>,
    pub status: CheckStatus,
    pub created_at: i64,
    pub metadata_json: Value,
}

/// A logs/metrics query that produced evidence. The raw logs are never stored;
/// only the query text and a redacted summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidenceQuery {
    pub id: String,
    pub check_id: String,
    pub source: SourceKind,
    pub query_type: String,
    pub query_text: String,
    pub time_from: Option<i64>,
    pub time_to: Option<i64>,
    pub result_summary_json: Value,
    pub executed_at: i64,
}

/// A command receipt: what ran, when, and how it exited. Recorded by
/// `truth run -- <cmd>` (or `truth record-run` from hooks). The evidence
/// source that makes "tests pass" checkable instead of refused.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    pub id: String,
    pub command: String,
    /// "test" | "build" | "lint" | "typecheck" | "other"
    pub kind: String,
    pub exit_code: i64,
    pub started_at: i64,
    pub finished_at: i64,
    pub duration_ms: Option<i64>,
    pub output_digest: Option<String>,
    /// Short, redacted tail of the combined output (never the raw logs).
    pub output_tail: Option<String>,
    pub metadata_json: Value,
}

/// The final cited answer for a check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Verdict {
    pub id: String,
    pub check_id: String,
    pub status: VerdictStatus,
    pub confidence: f32,
    pub summary: String,
    pub caveats_json: Value,
    pub evidence_ids_json: Value,
    pub suggested_action: Option<String>,
    pub created_at: i64,
}
