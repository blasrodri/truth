//! `truth docs <subject>` — is X present in the documentation, and is it
//! consistent with the code? Answers documentation coverage and **doc drift**
//! (the spec's `DocumentationAccuracy` question type).
//!
//! Symmetric to `truth uses` (code references): scans only the indexed
//! documentation files (markdown / rst / txt / README), then compares
//! doc-presence vs code-presence to classify drift.

use crate::config_util::{load_config, print_json};
use crate::refs::{scan_references, RefHit};
use anyhow::Result;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct DocsReport {
    pub subject: String,
    /// documented | undocumented | drift | absent
    pub status: String,
    /// Mentions in documentation files.
    pub doc_count: usize,
    /// References in code files.
    pub code_count: usize,
    pub doc_samples: Vec<RefHit>,
    pub caveats: Vec<String>,
}

/// Build a docs/drift report for a subject by scanning doc files for mentions
/// and code files for references, then comparing.
pub fn build_report(conn: &rusqlite::Connection, subject: &str) -> Result<DocsReport> {
    let doc_files = truth_db::repo::doc_file_uris(conn)?;
    // Code presence must exclude docs, else a docs-only mention looks like code.
    let code_files = truth_db::repo::code_file_uris(conn)?;

    let (doc_count, doc_samples, _) = scan_references(&doc_files, subject, None, None, 5);
    let (code_count, _, _) = scan_references(&code_files, subject, None, None, 0);

    let in_docs = doc_count > 0;
    let in_code = code_count > 0;

    let (status, mut caveats): (&str, Vec<String>) = match (in_docs, in_code) {
        (true, true) => (
            "documented",
            vec!["Mentioned in both docs and code.".to_string()],
        ),
        (true, false) => (
            "drift",
            vec![
                "Mentioned in documentation but NOT found in code — possible doc drift (a documented thing that no longer exists)."
                    .to_string(),
            ],
        ),
        (false, true) => (
            "undocumented",
            vec!["Present in code but not mentioned in any indexed doc.".to_string()],
        ),
        (false, false) => (
            "absent",
            vec!["Not found in docs or code (check spelling, or `truth index .`).".to_string()],
        ),
    };
    caveats.push("Text mentions in indexed documentation files (md/rst/txt/README).".to_string());

    Ok(DocsReport {
        subject: subject.to_string(),
        status: status.to_string(),
        doc_count,
        code_count,
        doc_samples,
        caveats,
    })
}

/// `truth docs <subject> [--json]`.
pub fn docs(subject: &str, json: bool) -> Result<()> {
    let config = load_config()?;
    let conn = truth_db::open(&config.database.path)?;
    let report = build_report(&conn, subject)?;

    if json {
        print_json(&serde_json::to_value(&report)?);
        return Ok(());
    }

    let headline = match report.status.as_str() {
        "documented" => format!("`{}` is documented ({} mention(s)).", subject, report.doc_count),
        "drift" => format!("`{}` is in the docs but NOT in the code (possible drift).", subject),
        "undocumented" => format!("`{}` is in the code but undocumented.", subject),
        _ => format!("`{}` was not found in docs or code.", subject),
    };
    println!("{headline}\n");

    if !report.doc_samples.is_empty() {
        println!("In documentation:");
        for h in &report.doc_samples {
            println!("  {}:{}  {}", h.file, h.line, h.text);
        }
    }
    println!(
        "\nPresence: docs={}, code={}",
        report.doc_count, report.code_count
    );
    if !report.caveats.is_empty() {
        println!("\nCaveats:");
        for c in &report.caveats {
            println!("• {c}");
        }
    }
    Ok(())
}
