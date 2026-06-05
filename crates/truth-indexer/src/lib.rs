//! `truth-indexer` — walk a repo, extract deterministic evidence, persist it.

pub mod extract;
pub mod walker;

use anyhow::{Context, Result};
use rayon::prelude::*;
use rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::path::Path;
use truth_core::config::RepoConfig;
use truth_core::enums::*;
use truth_core::models::*;
use truth_core::{new_id, now_secs};

pub struct IndexStats {
    /// Files the walker selected for indexing.
    pub files: usize,
    /// Files actually read as UTF-8 (binary/unreadable files are skipped).
    pub files_read: usize,
    pub artifacts: usize,
    pub evidence_items: usize,
    /// Wall-clock time spent indexing.
    pub elapsed: std::time::Duration,
}

impl IndexStats {
    pub fn files_per_sec(&self) -> f64 {
        let secs = self.elapsed.as_secs_f64();
        if secs > 0.0 {
            self.files as f64 / secs
        } else {
            f64::INFINITY
        }
    }

    pub fn evidence_per_file(&self) -> f64 {
        if self.files_read > 0 {
            self.evidence_items as f64 / self.files_read as f64
        } else {
            0.0
        }
    }
}

/// Index the repo rooted at `cfg.root` (overridable via `root_override`),
/// replacing previously indexed repo evidence.
pub fn index_repo(
    conn: &Connection,
    cfg: &RepoConfig,
    root_override: Option<&Path>,
) -> Result<IndexStats> {
    let start = std::time::Instant::now();
    let root = root_override.unwrap_or_else(|| Path::new(&cfg.root));
    truth_db::repo::clear_repo_evidence(conn)?;

    let files = walker::walk(root, cfg);
    let selected = files.len();
    let t_walk = start.elapsed();
    let t0 = std::time::Instant::now();

    // Phase 1 (parallel): read + extract each file independently. This is pure
    // CPU/IO with no shared state, so it scales across cores via rayon. rusqlite
    // is not Sync, so no DB work happens here.
    let built: Vec<FileBuild> = files.par_iter().filter_map(|path| build_file(path)).collect();
    let t_build = t0.elapsed();
    let t1 = std::time::Instant::now();

    // Phase 2 (serial): write everything in one transaction — turns thousands of
    // fsync-bound INSERTs into a single commit (the dominant cost at scale).
    let mut stats = IndexStats {
        files: selected,
        files_read: built.len(),
        artifacts: 0,
        evidence_items: 0,
        elapsed: std::time::Duration::ZERO,
    };
    // Bulk-load pragmas: indexing always clears and rebuilds, so durability of
    // intermediate writes does not matter — trade it for speed.
    conn.execute_batch(
        "PRAGMA synchronous = OFF;
         PRAGMA journal_mode = MEMORY;
         PRAGMA temp_store = MEMORY;",
    )?;
    conn.execute_batch("BEGIN")?;
    for fb in &built {
        truth_db::repo::insert_artifact(conn, &fb.artifact)
            .with_context(|| format!("inserting artifact for {}", fb.artifact.uri))?;
        stats.artifacts += 1;
        for (span, item) in &fb.evidence {
            truth_db::repo::insert_span(conn, span)?;
            truth_db::repo::insert_evidence(conn, item)?;
            stats.evidence_items += 1;
        }
    }
    conn.execute_batch("COMMIT")?;

    if std::env::var("TRUTH_PROFILE").is_ok() {
        eprintln!(
            "[profile] walk={:.0}ms build(parallel)={:.0}ms write={:.0}ms",
            t_walk.as_secs_f64() * 1000.0,
            t_build.as_secs_f64() * 1000.0,
            t1.elapsed().as_secs_f64() * 1000.0,
        );
    }

    stats.elapsed = start.elapsed();
    Ok(stats)
}

/// A fully-built (artifact + spans/evidence) file, ready for serial insertion.
struct FileBuild {
    artifact: Artifact,
    evidence: Vec<(Span, EvidenceItem)>,
}

/// Read one file and extract its evidence. Returns `None` for binary/unreadable
/// files. Pure: no DB access, safe to call in parallel.
fn build_file(path: &Path) -> Option<FileBuild> {
    let contents = std::fs::read_to_string(path).ok()?;
    let hash = hex_sha256(&contents);
    let uri = path.to_string_lossy().into_owned();
    let artifact = Artifact {
        id: new_id(),
        source: SourceKind::GitRepo,
        kind: artifact_kind_for(path),
        uri: uri.clone(),
        external_id: None,
        hash: Some(hash),
        authored_at: None,
        observed_at: now_secs(),
        author: None,
        metadata_json: serde_json::json!({}),
    };

    let mut evidence = Vec::new();
    for fact in extract::extract_file(path, &contents) {
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
            authority: authority_for(&fact.predicate, path),
            valid_from: None,
            valid_to: None,
            extraction_method: ExtractionMethod::Deterministic,
            metadata_json: serde_json::json!({ "uri": uri, "line": fact.line }),
        };
        evidence.push((span, item));
    }
    Some(FileBuild { artifact, evidence })
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
