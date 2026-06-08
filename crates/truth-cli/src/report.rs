//! `truth report` — run checks from a claim file and render a report.
//!
//! The runner (`run_report`) is shared with `truth ci`. It never fails because a
//! claim is contradicted; it only errors on operational problems.

use crate::check::run_check;
use crate::config_util::load_config;
use anyhow::{Context, Result};
use std::path::Path;
use truth_core::config::Config;
use truth_core::enums::Trigger;
use truth_core::report::{ClaimFile, RenderedEvidence, Report, ReportResult, ReportSummary};

/// Output format for report/ci.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Text,
    Markdown,
    Json,
}

impl Format {
    pub fn parse(s: &str) -> Result<Format> {
        match s.to_ascii_lowercase().as_str() {
            "text" => Ok(Format::Text),
            "markdown" | "md" => Ok(Format::Markdown),
            "json" => Ok(Format::Json),
            other => anyhow::bail!("unknown format `{other}` (text|markdown|json)"),
        }
    }
}

/// Read and parse a claim file (v1 `claims:` format).
pub fn load_claim_file(path: &str) -> Result<ClaimFile> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading claim file {path}"))?;
    ClaimFile::from_yaml(&text).with_context(|| format!("parsing claim file {path}"))
}

/// Run every claim in the file and assemble a report. `generated_at` is passed
/// in so the runner stays deterministic/testable (no clock access here).
pub fn run_report(config: &Config, claim_file: &ClaimFile, generated_at: &str) -> Result<Report> {
    let defaults = &claim_file.defaults;
    let mut results = Vec::new();
    let mut summary = ReportSummary::default();

    for claim in &claim_file.claims {
        let conn = truth_db::open_in_memory()?;
        if let Some(repo) = claim.repo(defaults) {
            truth_indexer::index_repo(&conn, &config.repo, Some(Path::new(repo)))
                .with_context(|| format!("indexing repo for claim `{}`", claim.id))?;
        }
        let local_log = claim.local_log(defaults);
        let outcome = run_check(&conn, config, &claim.text, Trigger::Cli, local_log)?;

        summary.tally(outcome.decision.status);
        results.push(ReportResult {
            id: claim.id.clone(),
            text: claim.text.clone(),
            severity: claim.severity,
            tags: claim.tags.clone(),
            status: outcome.decision.status.as_db_str().to_string(),
            confidence: outcome.decision.confidence,
            check_id: outcome.check_id.clone(),
            summary: concise_summary(&outcome.response_text),
            evidence: outcome.evidence.iter().map(to_rendered).collect(),
            caveats: outcome.decision.caveats.clone(),
        });
    }

    Ok(Report {
        generated_at: generated_at.to_string(),
        claims_checked: claim_file.claims.len(),
        summary,
        results,
    })
}

fn to_rendered(e: &crate::service::EvidenceJson) -> RenderedEvidence {
    RenderedEvidence {
        source: e.source.clone(),
        kind: e.kind.clone(),
        subject: e.subject.clone(),
        value: e.value.clone(),
        citation: e.citation.clone(),
    }
}

/// First non-empty line after the headline of a rendered response block.
pub fn concise_summary(response: &str) -> String {
    let mut lines = response.lines().filter(|l| !l.trim().is_empty());
    let _headline = lines.next();
    lines
        .next()
        .or_else(|| response.lines().next())
        .unwrap_or("")
        .trim()
        .to_string()
}

fn title(db_status: &str) -> String {
    db_status
        .split('_')
        .map(|w| {
            let mut c = w.chars();
            c.next()
                .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Render a report as plain text.
pub fn render_text(report: &Report) -> String {
    let mut out = String::new();
    out.push_str("truth report\n\n");
    out.push_str(&format!("Claims checked: {}\n\n", report.claims_checked));
    out.push_str("Summary:\n");
    out.push_str(&summary_lines(report));
    out.push_str("\nFindings:\n");
    for r in &report.results {
        out.push_str(&format!(
            "\n[{}] {} — {}\n",
            r.severity.as_str(),
            r.id,
            title(&r.status)
        ));
        out.push_str(&format!("  claim: {}\n", r.text));
        out.push_str(&format!("  {}\n", r.summary));
        for e in &r.evidence {
            out.push_str(&format!("  • {}\n", evidence_line(e)));
        }
        for c in &r.caveats {
            out.push_str(&format!("  caveat: {c}\n"));
        }
    }
    out.trim_end().to_string()
}

/// Render a report as GitHub-flavored markdown.
pub fn render_markdown(report: &Report) -> String {
    let s = &report.summary;
    let mut out = String::new();
    out.push_str("# truth report\n\n");
    out.push_str(&format!("Claims checked: {}\n\n", report.claims_checked));
    out.push_str("## Summary\n\n");
    out.push_str("| Status | Count |\n|---|---:|\n");
    out.push_str(&format!("| Supported | {} |\n", s.supported));
    out.push_str(&format!("| Contradicted | {} |\n", s.contradicted));
    out.push_str(&format!(
        "| Partially supported | {} |\n",
        s.partially_supported
    ));
    out.push_str(&format!("| Inconclusive | {} |\n", s.inconclusive));
    out.push_str(&format!(
        "| Needs more context | {} |\n",
        s.needs_more_context
    ));
    out.push_str("\n## Findings\n");
    for r in &report.results {
        out.push_str(&format!("\n### {} — {}\n\n", r.id, title(&r.status)));
        out.push_str("Claim:\n\n");
        out.push_str(&format!("> {}\n\n", r.text));
        out.push_str("Summary:\n\n");
        out.push_str(&format!("{}\n\n", r.summary));
        if !r.evidence.is_empty() {
            out.push_str("Evidence:\n\n");
            for e in &r.evidence {
                out.push_str(&format!("- {}\n", evidence_line(e)));
            }
            out.push('\n');
        }
        if !r.caveats.is_empty() {
            out.push_str("Caveats:\n\n");
            for c in &r.caveats {
                out.push_str(&format!("- {c}\n"));
            }
            out.push('\n');
        }
    }
    out.trim_end().to_string()
}

fn summary_lines(report: &Report) -> String {
    let s = &report.summary;
    let mut out = String::new();
    out.push_str(&format!("  Supported: {}\n", s.supported));
    out.push_str(&format!("  Contradicted: {}\n", s.contradicted));
    if s.partially_supported > 0 {
        out.push_str(&format!(
            "  Partially supported: {}\n",
            s.partially_supported
        ));
    }
    out.push_str(&format!("  Inconclusive: {}\n", s.inconclusive));
    if s.needs_more_context > 0 {
        out.push_str(&format!("  Needs more context: {}\n", s.needs_more_context));
    }
    out
}

fn evidence_line(e: &RenderedEvidence) -> String {
    let val = e
        .value
        .as_ref()
        .map(|v| format!(" = {v}"))
        .unwrap_or_default();
    let cite = e
        .citation
        .as_ref()
        .map(|c| format!(" ({c})"))
        .unwrap_or_default();
    let subj = e
        .subject
        .as_ref()
        .map(|s| format!(" {s}"))
        .unwrap_or_default();
    format!("{} {}{subj}{val}{cite}", e.source, e.kind)
}

/// Render to the requested format.
pub fn render(report: &Report, format: Format) -> Result<String> {
    Ok(match format {
        Format::Text => render_text(report),
        Format::Markdown => render_markdown(report),
        Format::Json => serde_json::to_string_pretty(report)?,
    })
}

/// CLI entry point for `truth report`.
pub fn report(
    claim_path: &str,
    local_log: Option<&str>,
    format: &str,
    out: Option<&str>,
) -> Result<()> {
    let format = Format::parse(format)?;
    let mut config = load_config()?;
    config.loki.enabled = false; // deterministic offline path

    let mut claim_file = load_claim_file(claim_path)?;
    // A `--local-log` flag overrides the file default for all claims.
    if let Some(ll) = local_log {
        claim_file.defaults.local_log = Some(ll.to_string());
    }

    let report = run_report(&config, &claim_file, &truth_core_now())?;
    let rendered = render(&report, format)?;

    if let Some(path) = out {
        std::fs::write(path, format!("{rendered}\n")).with_context(|| format!("writing {path}"))?;
        println!("Wrote report to {path}");
    } else {
        println!("{rendered}");
    }
    Ok(())
}

/// Current time as an RFC3339 string (kept out of `run_report` for determinism).
pub fn truth_core_now() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}
