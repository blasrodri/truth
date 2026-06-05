//! Typed persistence over the SQLite schema. Enums are stored as their
//! `as_db_str()` form; JSON columns as serialized strings.

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Row};
use std::str::FromStr;
use truth_core::enums::*;
use truth_core::models::*;

fn js(v: &serde_json::Value) -> String {
    v.to_string()
}

pub fn insert_artifact(conn: &Connection, a: &Artifact) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO artifacts
         (id, source, kind, uri, external_id, hash, authored_at, observed_at, author, metadata_json)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
        params![
            a.id,
            a.source.as_db_str(),
            a.kind.as_db_str(),
            a.uri,
            a.external_id,
            a.hash,
            a.authored_at,
            a.observed_at,
            a.author,
            js(&a.metadata_json),
        ],
    )?;
    Ok(())
}

pub fn insert_span(conn: &Connection, s: &Span) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO spans
         (id, artifact_id, text, start_line, end_line, start_byte, end_byte, metadata_json)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
        params![
            s.id,
            s.artifact_id,
            s.text,
            s.start_line,
            s.end_line,
            s.start_byte,
            s.end_byte,
            js(&s.metadata_json),
        ],
    )?;
    Ok(())
}

pub fn insert_evidence(conn: &Connection, e: &EvidenceItem) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO evidence_items
         (id, span_id, evidence_type, subject_text, subject_concept_id, predicate, object_text,
          value_json, unit, confidence, authority, valid_from, valid_to, extraction_method, metadata_json)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
        params![
            e.id,
            e.span_id,
            e.evidence_type.as_db_str(),
            e.subject_text,
            e.subject_concept_id,
            e.predicate,
            e.object_text,
            e.value_json.as_ref().map(|v| v.to_string()),
            e.unit,
            e.confidence,
            e.authority.as_db_str(),
            e.valid_from,
            e.valid_to,
            e.extraction_method.as_db_str(),
            js(&e.metadata_json),
        ],
    )?;
    Ok(())
}

use rusqlite::types::Value as SqlValue;

/// Build `INSERT OR REPLACE INTO <table> (<cols>) VALUES (?,?..),(?,?..),...` for
/// `rows` rows of `ncols` columns each.
fn multi_row_sql(table: &str, cols: &str, ncols: usize, rows: usize) -> String {
    let one = format!("({})", vec!["?"; ncols].join(","));
    let groups = vec![one.as_str(); rows].join(",");
    format!("INSERT OR REPLACE INTO {table} ({cols}) VALUES {groups}")
}

/// Insert `items` flattened into `ncols`-wide rows using chunked multi-row
/// INSERTs (one `execute` per ~`SQLITE_MAX_VARS/ncols` rows instead of per row).
/// `row_of` pushes one row's column values onto the buffer. Caller wraps in a
/// transaction.
fn bulk_insert<T>(
    conn: &Connection,
    table: &str,
    cols: &str,
    ncols: usize,
    items: &[T],
    mut row_of: impl FnMut(&T, &mut Vec<SqlValue>),
) -> Result<()> {
    if items.is_empty() {
        return Ok(());
    }
    // Stay well under SQLite's default 999-bound-variable limit.
    let max_rows = (900 / ncols).max(1);
    let mut buf: Vec<SqlValue> = Vec::with_capacity(max_rows * ncols);

    for chunk in items.chunks(max_rows) {
        buf.clear();
        for it in chunk {
            row_of(it, &mut buf);
        }
        let sql = multi_row_sql(table, cols, ncols, chunk.len());
        let mut stmt = conn.prepare_cached(&sql)?;
        stmt.execute(rusqlite::params_from_iter(buf.iter()))?;
    }
    Ok(())
}

fn s(v: &str) -> SqlValue {
    SqlValue::Text(v.to_string())
}
fn os(v: &Option<String>) -> SqlValue {
    match v {
        Some(x) => SqlValue::Text(x.clone()),
        None => SqlValue::Null,
    }
}
fn oi(v: Option<i64>) -> SqlValue {
    v.map(SqlValue::Integer).unwrap_or(SqlValue::Null)
}
fn ou(v: Option<u32>) -> SqlValue {
    v.map(|x| SqlValue::Integer(x as i64)).unwrap_or(SqlValue::Null)
}

/// Bulk-insert artifacts via chunked multi-row INSERTs. Caller wraps in a tx.
pub fn insert_artifacts(conn: &Connection, items: &[&Artifact]) -> Result<()> {
    bulk_insert(
        conn,
        "artifacts",
        "id, source, kind, uri, external_id, hash, authored_at, observed_at, author, metadata_json",
        10,
        items,
        |a, buf| {
            buf.push(s(&a.id));
            buf.push(s(a.source.as_db_str()));
            buf.push(s(a.kind.as_db_str()));
            buf.push(s(&a.uri));
            buf.push(os(&a.external_id));
            buf.push(os(&a.hash));
            buf.push(oi(a.authored_at));
            buf.push(SqlValue::Integer(a.observed_at));
            buf.push(os(&a.author));
            buf.push(s(&js(&a.metadata_json)));
        },
    )
}

/// Bulk-insert spans via chunked multi-row INSERTs. Caller wraps in a tx.
pub fn insert_spans(conn: &Connection, items: &[&Span]) -> Result<()> {
    bulk_insert(
        conn,
        "spans",
        "id, artifact_id, text, start_line, end_line, start_byte, end_byte, metadata_json",
        8,
        items,
        |sp, buf| {
            buf.push(s(&sp.id));
            buf.push(s(&sp.artifact_id));
            buf.push(s(&sp.text));
            buf.push(ou(sp.start_line));
            buf.push(ou(sp.end_line));
            buf.push(ou(sp.start_byte));
            buf.push(ou(sp.end_byte));
            buf.push(s(&js(&sp.metadata_json)));
        },
    )
}

/// Bulk-insert evidence items via chunked multi-row INSERTs. Caller wraps in a tx.
pub fn insert_evidence_items(conn: &Connection, items: &[&EvidenceItem]) -> Result<()> {
    bulk_insert(
        conn,
        "evidence_items",
        "id, span_id, evidence_type, subject_text, subject_concept_id, predicate, object_text, \
         value_json, unit, confidence, authority, valid_from, valid_to, extraction_method, metadata_json",
        15,
        items,
        |e, buf| {
            buf.push(s(&e.id));
            buf.push(s(&e.span_id));
            buf.push(s(e.evidence_type.as_db_str()));
            buf.push(os(&e.subject_text));
            buf.push(os(&e.subject_concept_id));
            buf.push(os(&e.predicate));
            buf.push(os(&e.object_text));
            buf.push(os(&e.value_json.as_ref().map(|v| v.to_string())));
            buf.push(os(&e.unit));
            buf.push(SqlValue::Real(e.confidence as f64));
            buf.push(s(e.authority.as_db_str()));
            buf.push(oi(e.valid_from));
            buf.push(oi(e.valid_to));
            buf.push(s(e.extraction_method.as_db_str()));
            buf.push(s(&js(&e.metadata_json)));
        },
    )
}

pub fn insert_check(conn: &Connection, c: &Check) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO checks
         (id, trigger, question, question_type, status, created_at, metadata_json)
         VALUES (?1,?2,?3,?4,?5,?6,?7)",
        params![
            c.id,
            c.trigger.as_db_str(),
            c.question,
            c.question_type.map(|q| q.as_db_str()),
            c.status.as_db_str(),
            c.created_at,
            js(&c.metadata_json),
        ],
    )?;
    Ok(())
}

pub fn insert_evidence_query(conn: &Connection, q: &EvidenceQuery) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO evidence_queries
         (id, check_id, source, query_type, query_text, time_from, time_to, result_summary_json, executed_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
        params![
            q.id,
            q.check_id,
            q.source.as_db_str(),
            q.query_type,
            q.query_text,
            q.time_from,
            q.time_to,
            js(&q.result_summary_json),
            q.executed_at,
        ],
    )?;
    Ok(())
}

pub fn insert_verdict(conn: &Connection, v: &Verdict) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO verdicts
         (id, check_id, status, confidence, summary, caveats_json, evidence_ids_json, suggested_action, created_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
        params![
            v.id,
            v.check_id,
            v.status.as_db_str(),
            v.confidence,
            v.summary,
            js(&v.caveats_json),
            js(&v.evidence_ids_json),
            v.suggested_action,
            v.created_at,
        ],
    )?;
    Ok(())
}

/// All evidence items whose subject text matches (used by repo query types).
pub fn evidence_by_subject(conn: &Connection, subject: &str) -> Result<Vec<EvidenceItem>> {
    let mut stmt = conn.prepare(
        "SELECT id, span_id, evidence_type, subject_text, subject_concept_id, predicate, object_text,
                value_json, unit, confidence, authority, valid_from, valid_to, extraction_method, metadata_json
         FROM evidence_items WHERE subject_text = ?1",
    )?;
    let rows = stmt
        .query_map([subject], row_to_evidence)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// All evidence items with a given predicate (e.g. "port", "retry_count").
pub fn evidence_by_predicate(conn: &Connection, predicate: &str) -> Result<Vec<EvidenceItem>> {
    let mut stmt = conn.prepare(
        "SELECT id, span_id, evidence_type, subject_text, subject_concept_id, predicate, object_text,
                value_json, unit, confidence, authority, valid_from, valid_to, extraction_method, metadata_json
         FROM evidence_items WHERE predicate = ?1",
    )?;
    let rows = stmt
        .query_map([predicate], row_to_evidence)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Evidence whose subject OR predicate matches `key` (case-insensitive),
/// used by `truth config` to find code/config definitions by key or concept.
pub fn evidence_matching_key(conn: &Connection, key: &str) -> Result<Vec<EvidenceItem>> {
    let like = format!("%{}%", key.to_lowercase());
    let mut stmt = conn.prepare(
        "SELECT id, span_id, evidence_type, subject_text, subject_concept_id, predicate, object_text,
                value_json, unit, confidence, authority, valid_from, valid_to, extraction_method, metadata_json
         FROM evidence_items
         WHERE lower(subject_text) LIKE ?1 OR lower(predicate) LIKE ?1
         ORDER BY predicate, subject_text",
    )?;
    let rows = stmt
        .query_map([like], row_to_evidence)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// All indexed evidence items, ordered by predicate then subject. Used by
/// `inspect` / `baseline` to summarize what the repo contains.
pub fn all_evidence(conn: &Connection) -> Result<Vec<EvidenceItem>> {
    let mut stmt = conn.prepare(
        "SELECT id, span_id, evidence_type, subject_text, subject_concept_id, predicate, object_text,
                value_json, unit, confidence, authority, valid_from, valid_to, extraction_method, metadata_json
         FROM evidence_items
         ORDER BY predicate, subject_text",
    )?;
    let rows = stmt
        .query_map([], row_to_evidence)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// All indexed repo file URIs (the corpus a reference finder re-reads). Code
/// files only (git_repo source).
pub fn repo_file_uris(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT uri FROM artifacts WHERE source = 'git_repo' ORDER BY uri",
    )?;
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Row counts for the three indexable tables (artifacts, spans, evidence).
#[derive(Debug, Clone, Copy, Default)]
pub struct IndexCounts {
    pub artifacts: i64,
    pub spans: i64,
    pub evidence_items: i64,
}

fn count_rows(conn: &Connection, table: &str) -> Result<i64> {
    // `table` is never user-supplied; it comes from the fixed list below.
    let sql = format!("SELECT COUNT(*) FROM {table}");
    Ok(conn.query_row(&sql, [], |r| r.get(0))?)
}

/// Count rows in artifacts/spans/evidence_items.
pub fn index_counts(conn: &Connection) -> Result<IndexCounts> {
    Ok(IndexCounts {
        artifacts: count_rows(conn, "artifacts")?,
        spans: count_rows(conn, "spans")?,
        evidence_items: count_rows(conn, "evidence_items")?,
    })
}

/// Fetch a single check by id.
pub fn get_check(conn: &Connection, check_id: &str) -> Result<Option<Check>> {
    let c = conn
        .query_row(
            "SELECT id, trigger, question, question_type, status, created_at, metadata_json
             FROM checks WHERE id = ?1",
            [check_id],
            |r| {
                Ok(Check {
                    id: r.get(0)?,
                    trigger: parse_enum::<Trigger>(r, 1)?,
                    question: r.get(2)?,
                    question_type: {
                        let s: Option<String> = r.get(3)?;
                        s.and_then(|s| s.parse::<QuestionType>().ok())
                    },
                    status: parse_enum::<CheckStatus>(r, 4)?,
                    created_at: r.get(5)?,
                    metadata_json: parse_json(r, 6)?,
                })
            },
        )
        .optional()?;
    Ok(c)
}

/// All evidence queries recorded for a check, oldest first.
pub fn get_evidence_queries_for_check(
    conn: &Connection,
    check_id: &str,
) -> Result<Vec<EvidenceQuery>> {
    let mut stmt = conn.prepare(
        "SELECT id, check_id, source, query_type, query_text, time_from, time_to, result_summary_json, executed_at
         FROM evidence_queries WHERE check_id = ?1 ORDER BY executed_at ASC",
    )?;
    let rows = stmt
        .query_map([check_id], |r| {
            Ok(EvidenceQuery {
                id: r.get(0)?,
                check_id: r.get(1)?,
                source: parse_enum::<SourceKind>(r, 2)?,
                query_type: r.get(3)?,
                query_text: r.get(4)?,
                time_from: r.get(5)?,
                time_to: r.get(6)?,
                result_summary_json: parse_json(r, 7)?,
                executed_at: r.get(8)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn get_verdict_for_check(conn: &Connection, check_id: &str) -> Result<Option<Verdict>> {
    let v = conn
        .query_row(
            "SELECT id, check_id, status, confidence, summary, caveats_json, evidence_ids_json, suggested_action, created_at
             FROM verdicts WHERE check_id = ?1 ORDER BY created_at DESC LIMIT 1",
            [check_id],
            |r| {
                Ok(Verdict {
                    id: r.get(0)?,
                    check_id: r.get(1)?,
                    status: parse_enum::<VerdictStatus>(r, 2)?,
                    confidence: r.get(3)?,
                    summary: r.get(4)?,
                    caveats_json: parse_json(r, 5)?,
                    evidence_ids_json: parse_json(r, 6)?,
                    suggested_action: r.get(7)?,
                    created_at: r.get(8)?,
                })
            },
        )
        .optional()?;
    Ok(v)
}

fn parse_enum<T: FromStr>(r: &Row, idx: usize) -> rusqlite::Result<T> {
    let s: String = r.get(idx)?;
    T::from_str(&s).map_err(|_| {
        rusqlite::Error::FromSqlConversionFailure(
            idx,
            rusqlite::types::Type::Text,
            format!("bad enum value {s:?}").into(),
        )
    })
}

fn parse_json(r: &Row, idx: usize) -> rusqlite::Result<serde_json::Value> {
    let s: String = r.get(idx)?;
    serde_json::from_str(&s).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(idx, rusqlite::types::Type::Text, Box::new(e))
    })
}

fn parse_json_opt(r: &Row, idx: usize) -> rusqlite::Result<Option<serde_json::Value>> {
    let s: Option<String> = r.get(idx)?;
    match s {
        Some(s) => Ok(Some(serde_json::from_str(&s).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(idx, rusqlite::types::Type::Text, Box::new(e))
        })?)),
        None => Ok(None),
    }
}

fn row_to_evidence(r: &Row) -> rusqlite::Result<EvidenceItem> {
    Ok(EvidenceItem {
        id: r.get(0)?,
        span_id: r.get(1)?,
        evidence_type: parse_enum::<EvidenceType>(r, 2)?,
        subject_text: r.get(3)?,
        subject_concept_id: r.get(4)?,
        predicate: r.get(5)?,
        object_text: r.get(6)?,
        value_json: parse_json_opt(r, 7)?,
        unit: r.get(8)?,
        confidence: r.get(9)?,
        authority: parse_enum::<Authority>(r, 10)?,
        valid_from: r.get(11)?,
        valid_to: r.get(12)?,
        extraction_method: parse_enum::<ExtractionMethod>(r, 13)?,
        metadata_json: parse_json(r, 14)?,
    })
}

/// Delete all artifacts/spans/evidence (used to re-index cleanly).
pub fn clear_repo_evidence(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "DELETE FROM evidence_items;
         DELETE FROM spans;
         DELETE FROM artifacts WHERE source IN ('git_repo','local_logs');",
    )
    .context("clearing repo evidence")?;
    Ok(())
}

/// A previously-indexed repo file's change-detection state.
#[derive(Debug, Clone)]
pub struct PriorFile {
    pub artifact_id: String,
    pub hash: String,
    /// mtime (unix secs) and size (bytes) from the artifact metadata; either may
    /// be 0 for artifacts written before this was tracked.
    pub mtime: i64,
    pub size: u64,
}

/// Map of indexed repo-file `uri -> PriorFile`, used by incremental indexing to
/// detect unchanged / changed / deleted files via mtime+size, falling back to
/// the content hash.
pub fn repo_prior_files(conn: &Connection) -> Result<std::collections::HashMap<String, PriorFile>> {
    let mut stmt = conn.prepare(
        "SELECT uri, id, hash, metadata_json FROM artifacts
         WHERE source = 'git_repo' AND hash IS NOT NULL",
    )?;
    let mut map = std::collections::HashMap::new();
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
        ))
    })?;
    for row in rows {
        let (uri, id, hash, meta) = row?;
        let v: serde_json::Value = serde_json::from_str(&meta).unwrap_or_default();
        let mtime = v.get("mtime").and_then(|x| x.as_i64()).unwrap_or(0);
        let size = v.get("size").and_then(|x| x.as_u64()).unwrap_or(0);
        map.insert(uri, PriorFile { artifact_id: id, hash, mtime, size });
    }
    Ok(map)
}

/// Delete a single repo file's artifact and all its spans/evidence by artifact
/// id (used when a file changed or was removed). Caller wraps in a transaction.
pub fn delete_artifact_cascade(conn: &Connection, artifact_id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM evidence_items WHERE span_id IN (SELECT id FROM spans WHERE artifact_id = ?1)",
        [artifact_id],
    )?;
    conn.execute("DELETE FROM spans WHERE artifact_id = ?1", [artifact_id])?;
    conn.execute("DELETE FROM artifacts WHERE id = ?1", [artifact_id])?;
    Ok(())
}
