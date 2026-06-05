//! Source adapter traits (spec §12.1, §12.2).

use crate::models::{Artifact, EvidenceItem, Span};
use crate::query::{EvidenceQueryResult, EvidenceQuerySpec};

/// An adapter that ingests artifacts from a source and extracts spans/evidence.
pub trait SourceAdapter {
    fn ingest(&self) -> anyhow::Result<Vec<Artifact>>;
    fn extract_spans(&self, artifact: &Artifact) -> anyhow::Result<Vec<Span>>;
    fn extract_evidence(&self, span: &Span) -> anyhow::Result<Vec<EvidenceItem>>;
}

/// A runtime source (logs/metrics) that can answer safe query templates.
pub trait QueryableSource {
    fn execute_query(&self, spec: EvidenceQuerySpec) -> anyhow::Result<EvidenceQueryResult>;
}
