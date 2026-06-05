//! `truth-indexer` — walk a repo, extract deterministic evidence, persist it.

pub mod extract;
pub mod walker;

use anyhow::{Context, Result};
use rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::path::Path;
use truth_core::config::RepoConfig;
use truth_core::enums::*;
use truth_core::models::*;
use truth_core::{new_id, now_secs};

pub struct IndexStats {
    pub files: usize,
    pub artifacts: usize,
    pub evidence_items: usize,
}

/// Index the repo rooted at `cfg.root` (overridable via `root_override`),
/// replacing previously indexed repo evidence.
pub fn index_repo(
    conn: &Connection,
    cfg: &RepoConfig,
    root_override: Option<&Path>,
) -> Result<IndexStats> {
    let root = root_override.unwrap_or_else(|| Path::new(&cfg.root));
    truth_db::repo::clear_repo_evidence(conn)?;

    let files = walker::walk(root, cfg);
    let mut stats = IndexStats {
        files: files.len(),
        artifacts: 0,
        evidence_items: 0,
    };

    for path in files {
        let contents = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue, // skip non-utf8/binary
        };
        let hash = hex_sha256(&contents);
        let artifact = Artifact {
            id: new_id(),
            source: SourceKind::GitRepo,
            kind: artifact_kind_for(&path),
            uri: path.to_string_lossy().into_owned(),
            external_id: None,
            hash: Some(hash),
            authored_at: None,
            observed_at: now_secs(),
            author: None,
            metadata_json: serde_json::json!({}),
        };
        truth_db::repo::insert_artifact(conn, &artifact)
            .with_context(|| format!("inserting artifact for {}", path.display()))?;
        stats.artifacts += 1;

        for fact in extract::extract_file(&path, &contents) {
            let span = Span {
                id: new_id(),
                artifact_id: artifact.id.clone(),
                text: fact.text.clone(),
                start_line: Some(fact.line),
                end_line: Some(fact.line),
                start_byte: None,
                end_byte: None,
                metadata_json: serde_json::json!({}),
            };
            truth_db::repo::insert_span(conn, &span)?;

            let item = EvidenceItem {
                id: new_id(),
                span_id: span.id.clone(),
                evidence_type: EvidenceType::Definition,
                subject_text: Some(fact.subject.clone()),
                subject_concept_id: None,
                predicate: Some(fact.predicate.clone()),
                object_text: None,
                value_json: Some(fact.value.clone()),
                unit: None,
                confidence: 1.0,
                authority: authority_for(&fact.predicate, &path),
                valid_from: None,
                valid_to: None,
                extraction_method: ExtractionMethod::Deterministic,
                metadata_json: serde_json::json!({ "uri": artifact.uri, "line": fact.line }),
            };
            truth_db::repo::insert_evidence(conn, &item)?;
            stats.evidence_items += 1;
        }
    }

    Ok(stats)
}

fn hex_sha256(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    let digest = h.finalize();
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn artifact_kind_for(path: &Path) -> ArtifactKind {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    match ext {
        "md" | "markdown" => ArtifactKind::MarkdownDoc,
        "toml" | "yaml" | "yml" | "json" | "env" | "ini" | "conf" => ArtifactKind::ConfigFile,
        _ => {
            if path.file_name().and_then(|f| f.to_str()) == Some("docker-compose.yml") {
                ArtifactKind::ConfigFile
            } else {
                ArtifactKind::SourceFile
            }
        }
    }
}

fn authority_for(predicate: &str, path: &Path) -> Authority {
    let is_doc = matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("md") | Some("markdown")
    );
    if is_doc {
        return Authority::OfficialDoc;
    }
    match predicate {
        "port" | "config_value" | "dependency_exists" => Authority::Config,
        _ => Authority::Code,
    }
}
