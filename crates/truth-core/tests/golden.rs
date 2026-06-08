//! Golden verdict tests (spec §19): fixed claim + evidence → expected status.

use serde_json::json;
use truth_core::claim::{ClaimOperator, ClaimType, StructuredClaim};
use truth_core::enums::*;
use truth_core::models::EvidenceItem;
use truth_core::query::{EvidenceQueryResult, QueryType};
use truth_core::verdict::{decide, VerdictInput};

fn claim(ct: ClaimType, value: serde_json::Value) -> StructuredClaim {
    StructuredClaim {
        is_checkable: true,
        claim_type: ct,
        subject: Some("/v1/checkout".into()),
        predicate: None,
        operator: ClaimOperator::Equals,
        value: Some(value),
        unit: None,
        time_window: Some("7d".into()),
        environment: Some("prod".into()),
        confidence: 0.85,
        needs_clarification: false,
        clarification_question: None,
    }
}

fn route_count(n: i64) -> EvidenceQueryResult {
    EvidenceQueryResult {
        source: SourceKind::Loki,
        query_type: QueryType::RouteCount,
        query_text: "sum(...)".into(),
        count: Some(n),
        latest_seen: Some(1_717_509_780),
        redacted_samples: vec![],
        time_from: None,
        time_to: None,
        extra: json!({}),
    }
}

fn def(predicate: &str, value: f64) -> EvidenceItem {
    EvidenceItem {
        id: "e".into(),
        span_id: "s".into(),
        evidence_type: EvidenceType::Definition,
        subject_text: None,
        subject_concept_id: None,
        predicate: Some(predicate.into()),
        object_text: None,
        value_json: Some(json!(value)),
        unit: None,
        confidence: 1.0,
        authority: Authority::Code,
        valid_from: None,
        valid_to: None,
        extraction_method: ExtractionMethod::Deterministic,
        metadata_json: json!({}),
    }
}

#[test]
fn golden_usage_contradicted() {
    let c = claim(ClaimType::UsageCount, json!(0));
    let results = [route_count(12481)];
    let d = decide(&VerdictInput {
        claim: &c,
        items: &[],
        query_results: &results,
        usage_threshold: 0,
            code_references: None,
            symbol_status: None,
    });
    assert_eq!(d.status, VerdictStatus::Contradicted);
}

#[test]
fn golden_retry_mismatch_contradicted() {
    let c = claim(ClaimType::RetryCount, json!(3));
    let items = [def("retry_count", 5.0)];
    let d = decide(&VerdictInput {
        claim: &c,
        items: &items,
        query_results: &[],
        usage_threshold: 0,
            code_references: None,
            symbol_status: None,
    });
    assert_eq!(d.status, VerdictStatus::Contradicted);
}

#[test]
fn golden_port_supported() {
    let c = claim(ClaimType::ConfigValue, json!(8080));
    let items = [def("port", 8080.0)];
    let d = decide(&VerdictInput {
        claim: &c,
        items: &items,
        query_results: &[],
        usage_threshold: 0,
            code_references: None,
            symbol_status: None,
    });
    assert_eq!(d.status, VerdictStatus::Supported);
}

#[test]
fn golden_no_evidence_inconclusive() {
    let c = claim(ClaimType::UsageCount, json!(0));
    let d = decide(&VerdictInput {
        claim: &c,
        items: &[],
        query_results: &[],
        usage_threshold: 0,
            code_references: None,
            symbol_status: None,
    });
    assert_eq!(d.status, VerdictStatus::Inconclusive);
}
