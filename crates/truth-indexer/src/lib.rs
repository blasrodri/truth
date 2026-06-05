//! `truth-indexer` — walk a repo, extract deterministic evidence, persist it.

pub mod extract;
pub mod walker;

use anyhow::Result;
use rayon::prelude::*;
use rusqlite::Connection;
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

/// Full re-index: clear everything and rebuild. Used by eval/report (fresh
/// in-memory DBs) and by `truth index --full`.
pub fn index_repo(
    conn: &Connection,
    cfg: &RepoConfig,
    root_override: Option<&Path>,
) -> Result<IndexStats> {
    index_repo_opts(conn, cfg, root_override, false)
}

/// Index with an optional incremental fast-path.
///
/// When `incremental`, files whose content hash matches the previously indexed
/// hash are skipped entirely (no re-extraction, no writes); only changed/new
/// files are rebuilt and deleted files are pruned. On an unchanged repo this is
/// just one parallel hash pass — typically ~10-50ms even on a large repo.
pub fn index_repo_opts(
    conn: &Connection,
    cfg: &RepoConfig,
    root_override: Option<&Path>,
    incremental: bool,
) -> Result<IndexStats> {
    let start = std::time::Instant::now();
    let root = root_override.unwrap_or_else(|| Path::new(&cfg.root));

    // Prior state for incremental diffing (empty for a full rebuild).
    let prior = if incremental {
        truth_db::repo::repo_prior_files(conn)?
    } else {
        truth_db::repo::clear_repo_evidence(conn)?;
        std::collections::HashMap::new()
    };

    let files = walker::walk(root, cfg);
    let selected = files.len();
    let t_walk = start.elapsed();
    let t0 = std::time::Instant::now();

    // Phase 1 (parallel): for each file, first try the mtime+size fast path
    // (no read at all); only read + hash + extract changed/new files. rusqlite is
    // not Sync, so no DB work happens here.
    let outcomes: Vec<BuildOutcome> = files
        .par_iter()
        .filter_map(|wf| build_outcome(wf, &prior))
        .collect();
    let t_build = t0.elapsed();
    let t1 = std::time::Instant::now();

    // Compute the diff: which prior files survived (so we can prune deletions).
    let mut seen_uris = std::collections::HashSet::new();
    let mut changed_old_ids: Vec<String> = Vec::new();
    let mut built: Vec<&FileBuild> = Vec::new();
    let mut files_read = 0usize;
    for o in &outcomes {
        match o {
            BuildOutcome::Unchanged { uri } => {
                seen_uris.insert(uri.clone());
                files_read += 1;
            }
            BuildOutcome::Built { build, replaced } => {
                seen_uris.insert(build.artifact.uri.clone());
                if let Some(old_id) = replaced {
                    changed_old_ids.push(old_id.clone());
                }
                built.push(build.as_ref());
                files_read += 1;
            }
        }
    }
    // Files in the DB but no longer on disk (incremental only).
    let deleted_ids: Vec<String> = prior
        .iter()
        .filter(|(uri, _)| !seen_uris.contains(*uri))
        .map(|(_, pf)| pf.artifact_id.clone())
        .collect();

    let mut stats = IndexStats {
        files: selected,
        files_read,
        artifacts: 0,
        evidence_items: 0,
        elapsed: std::time::Duration::ZERO,
    };

    // Bulk-load pragmas: indexing rebuilds, so durability of intermediate writes
    // does not matter — trade it for speed.
    conn.execute_batch(
        "PRAGMA synchronous = OFF;
         PRAGMA journal_mode = MEMORY;
         PRAGMA temp_store = MEMORY;
         PRAGMA cache_size = -65536;",
    )?;

    // Flatten built files into column vectors for one prepared statement each.
    let artifacts: Vec<&Artifact> = built.iter().map(|fb| &fb.artifact).collect();
    let mut spans: Vec<&Span> = Vec::new();
    let mut items: Vec<&EvidenceItem> = Vec::new();
    for fb in &built {
        for (span, item) in &fb.evidence {
            spans.push(span);
            items.push(item);
        }
    }
    stats.artifacts = artifacts.len();
    stats.evidence_items = items.len();

    conn.execute_batch("BEGIN")?;
    // Prune changed (to be replaced) and deleted files first.
    for id in changed_old_ids.iter().chain(deleted_ids.iter()) {
        truth_db::repo::delete_artifact_cascade(conn, id)?;
    }
    truth_db::repo::insert_artifacts(conn, &artifacts)?;
    truth_db::repo::insert_spans(conn, &spans)?;
    truth_db::repo::insert_evidence_items(conn, &items)?;
    conn.execute_batch("COMMIT")?;

    if std::env::var("TRUTH_PROFILE").is_ok() {
        eprintln!(
            "[profile] walk={:.0}ms build(parallel)={:.0}ms write={:.0}ms  (incremental={}, rebuilt={}, deleted={})",
            t_walk.as_secs_f64() * 1000.0,
            t_build.as_secs_f64() * 1000.0,
            t1.elapsed().as_secs_f64() * 1000.0,
            incremental,
            built.len(),
            deleted_ids.len(),
        );
    }

    stats.elapsed = start.elapsed();
    Ok(stats)
}

/// Outcome of processing one file in the parallel phase.
enum BuildOutcome {
    /// File hash matched the prior index — nothing to do.
    Unchanged { uri: String },
    /// File is new or changed; `replaced` is the prior artifact id to delete.
    /// Boxed to keep the enum small (the `Unchanged` path is the common one in
    /// incremental re-indexing).
    Built { build: Box<FileBuild>, replaced: Option<String> },
}

/// Decide what to do with one walked file. Fast path: if the file's mtime+size
/// match the prior index, return `Unchanged` WITHOUT reading the file at all.
/// Otherwise read + hash (blake3) + extract.
fn build_outcome(
    wf: &walker::WalkedFile,
    prior: &std::collections::HashMap<String, truth_db::repo::PriorFile>,
) -> Option<BuildOutcome> {
    let uri = wf.path.to_string_lossy().into_owned();
    let known = prior.get(&uri);

    // Fast path: unchanged mtime AND size → skip read/hash/extract entirely.
    if let Some(pf) = known {
        if pf.mtime != 0 && pf.mtime == wf.mtime && pf.size == wf.size {
            return Some(BuildOutcome::Unchanged { uri });
        }
    }

    // Slow path: read + content hash. Confirms a real change (mtime can change
    // without content changing, e.g. `touch`).
    let contents = std::fs::read_to_string(&wf.path).ok()?;
    let hash = content_hash(&contents);

    if let Some(pf) = known {
        if pf.hash == hash {
            return Some(BuildOutcome::Unchanged { uri });
        }
        let build = Box::new(build_from(&wf.path, &uri, contents, hash, wf));
        return Some(BuildOutcome::Built { build, replaced: Some(pf.artifact_id.clone()) });
    }
    let build = Box::new(build_from(&wf.path, &uri, contents, hash, wf));
    Some(BuildOutcome::Built { build, replaced: None })
}

/// A fully-built (artifact + spans/evidence) file, ready for serial insertion.
struct FileBuild {
    artifact: Artifact,
    evidence: Vec<(Span, EvidenceItem)>,
}

/// Build an artifact + evidence from already-read file contents. Pure: no DB
/// access, safe to call in parallel. mtime+size are stored in metadata so the
/// next incremental index can skip this file without reading it.
fn build_from(
    path: &Path,
    uri: &str,
    contents: String,
    hash: String,
    wf: &walker::WalkedFile,
) -> FileBuild {
    let artifact = Artifact {
        id: new_id(),
        source: SourceKind::GitRepo,
        kind: artifact_kind_for(path),
        uri: uri.to_string(),
        external_id: None,
        hash: Some(hash),
        authored_at: None,
        observed_at: now_secs(),
        author: None,
        metadata_json: serde_json::json!({ "mtime": wf.mtime, "size": wf.size }),
    };

    let facts = extract::extract_file(path, &contents);
    let mut evidence = Vec::with_capacity(facts.len());
    for fact in facts {
        let authority = authority_for(&fact.predicate, path);
        let span = Span {
            id: new_id(),
            artifact_id: artifact.id.clone(),
            text: fact.text.to_string(),
            start_line: Some(fact.line),
            end_line: Some(fact.line),
            start_byte: None,
            end_byte: None,
            metadata_json: empty_json(),
        };
        // The enriched human label (e.g. for routes) is stored in `object_text`
        // — a previously-unused column that exactly fits "human description" —
        // so concept resolution can read it without parsing JSON.
        let label = fact.label;
        let mut metadata = serde_json::json!({ "uri": uri, "line": fact.line });
        if let Some(l) = &label {
            metadata["label"] = serde_json::Value::String(l.clone());
        }
        let item = EvidenceItem {
            id: new_id(),
            span_id: span.id.clone(),
            evidence_type: EvidenceType::Definition,
            // Move the borrowed/owned Cow into an owned String once, here.
            subject_text: Some(fact.subject.into_owned()),
            subject_concept_id: None,
            predicate: Some(fact.predicate.into_owned()),
            object_text: label,
            value_json: Some(fact.value),
            unit: None,
            confidence: 1.0,
            authority,
            valid_from: None,
            valid_to: None,
            extraction_method: ExtractionMethod::Deterministic,
            metadata_json: metadata,
        };
        evidence.push((span, item));
    }
    FileBuild { artifact, evidence }
}

/// An empty JSON object that does not allocate (serde's `Map::new` is lazy).
/// Cheaper than `serde_json::json!({})` in the per-span hot path.
fn empty_json() -> serde_json::Value {
    serde_json::Value::Object(serde_json::Map::new())
}

/// Content hash for change detection. blake3 is ~5-10x faster than sha256 and we
/// do not need cryptographic strength here — only "did the bytes change?".
fn content_hash(s: &str) -> String {
    blake3::hash(s.as_bytes()).to_hex().to_string()
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
