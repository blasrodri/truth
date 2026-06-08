//! `truth claims` — extract candidate engineering claims from docs/text into a
//! reviewable claim file. This does NOT verify claims; it generates YAML for a
//! human to curate.
//!
//! Extraction is deterministic (regex/keyword) by default; an optional LLM is
//! used only when configured. The LLM never decides truth — here it only helps
//! surface candidate sentences.

use crate::config_util::load_config;
use anyhow::{Context, Result};
use std::path::Path;
use truth_core::claim::ClaimType;
use truth_core::config::Config;
use truth_core::report::{
    ClaimDefaults, ClaimExtraction, ClaimFile, ClaimFileMetadata, ClaimSource, ClaimSpec, Severity,
};
use truth_llm::{ClaimExtractor, RegexExtractor};

/// A candidate claim found in a source file.
struct Candidate {
    text: String,
    file: String,
    line: u32,
    claim_type: ClaimType,
    confidence: f32,
}

/// Extract candidate claims from a set of files/dirs.
pub fn extract_claims(paths: &[String], config: &Config) -> Result<Vec<ClaimSpec>> {
    let files = collect_files(paths)?;
    let mut specs = Vec::new();
    let mut id_counts = std::collections::HashMap::new();

    for file in &files {
        let contents =
            std::fs::read_to_string(file).with_context(|| format!("reading {}", file))?;
        for cand in candidates_in(&contents, file, config) {
            let slug = slug_for(&cand);
            let counter = id_counts.entry(slug.clone()).or_insert(0);
            *counter += 1;
            let id = format!("{slug}-{counter}");
            specs.push(ClaimSpec {
                id,
                text: cand.text,
                severity: severity_for(cand.claim_type),
                tags: tags_for(cand.claim_type),
                expected_status: None,
                repo: None,
                local_log: None,
                env: None,
                window: None,
                source: Some(ClaimSource {
                    file: cand.file,
                    line: Some(cand.line),
                }),
                extraction: Some(ClaimExtraction {
                    method: if config.llm.enabled {
                        "llm".into()
                    } else {
                        "regex".into()
                    },
                    confidence: cand.confidence,
                }),
            });
        }
    }
    Ok(specs)
}

/// Find candidate claim lines in a document. Each non-trivial line is run
/// through the deterministic extractor; checkable lines become candidates.
fn candidates_in(contents: &str, file: &str, _config: &Config) -> Vec<Candidate> {
    let extractor = RegexExtractor;
    let mut out = Vec::new();
    for (i, raw) in contents.lines().enumerate() {
        let line = clean_markdown(raw);
        if !is_plausible(&line) {
            continue;
        }
        let claim = extractor.extract(&line);
        // Keep only concrete, checkable claims; skip vague prose.
        if claim.is_checkable && claim.claim_type != ClaimType::Unknown {
            out.push(Candidate {
                text: line,
                file: file.to_string(),
                line: (i + 1) as u32,
                claim_type: claim.claim_type,
                confidence: round2(claim.confidence),
            });
        }
    }
    out
}

/// Strip common Markdown decoration so the extractor sees plain prose.
fn clean_markdown(line: &str) -> String {
    let t = line.trim();
    let t = t.trim_start_matches(['#', '>', '-', '*', '+', ' ', '\t']);
    t.replace(['`', '*', '_'], "").trim().to_string()
}

/// A quick pre-filter so we don't run the extractor on headings/empty lines.
fn is_plausible(line: &str) -> bool {
    let words = line.split_whitespace().count();
    (4..=40).contains(&words)
}

fn severity_for(ct: ClaimType) -> Severity {
    match ct {
        ClaimType::ErrorStillHappening
        | ClaimType::UsageCount
        | ClaimType::RetryCount
        | ClaimType::TimeoutValue => Severity::Warning,
        _ => Severity::Info,
    }
}

fn tags_for(ct: ClaimType) -> Vec<String> {
    let t: &[&str] = match ct {
        ClaimType::UsageCount | ClaimType::RouteExists => &["usage"],
        ClaimType::ErrorStillHappening => &["errors"],
        ClaimType::RetryCount | ClaimType::TimeoutValue | ClaimType::ConfigValue => &["config"],
        ClaimType::DependencyUsed => &["dependency"],
        ClaimType::EnvVarExists => &["config", "env"],
        ClaimType::JobLastSuccess => &["jobs"],
        _ => &[],
    };
    t.iter().map(|s| s.to_string()).collect()
}

fn slug_for(c: &Candidate) -> String {
    let stem = Path::new(&c.file)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("doc")
        .to_lowercase();
    let kind = match c.claim_type {
        ClaimType::UsageCount => "usage",
        ClaimType::ErrorStillHappening => "error",
        ClaimType::RouteExists => "route",
        ClaimType::SymbolExists => "symbol",
        ClaimType::ConfigValue => "config",
        ClaimType::EnvVarExists => "env",
        ClaimType::DependencyUsed => "dependency",
        ClaimType::RetryCount => "retry-count",
        ClaimType::TimeoutValue => "timeout",
        ClaimType::VersionRequired => "version",
        ClaimType::JobLastSuccess => "job",
        ClaimType::LatestOccurrence => "latest",
        ClaimType::FeatureFlagEnabled => "flag",
        ClaimType::Unknown => "claim",
    };
    format!("{stem}-{kind}")
}

fn round2(f: f32) -> f32 {
    (f * 100.0).round() / 100.0
}

/// Recursively collect readable text/markdown files from the given paths.
fn collect_files(paths: &[String]) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for p in paths {
        let path = Path::new(p);
        if path.is_dir() {
            for entry in walkdir_min(path) {
                if is_text_file(&entry) {
                    out.push(entry);
                }
            }
        } else if path.is_file() {
            out.push(p.clone());
        } else {
            anyhow::bail!("path not found: {p}");
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

fn is_text_file(path: &str) -> bool {
    matches!(
        Path::new(path).extension().and_then(|e| e.to_str()),
        Some("md" | "markdown" | "txt" | "rst" | "adoc")
    ) || Path::new(path).file_name().and_then(|f| f.to_str()) == Some("README")
}

/// Minimal recursive directory walk (avoids adding walkdir to truth-cli).
fn walkdir_min(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                // Skip common noise dirs.
                let name = p.file_name().and_then(|f| f.to_str()).unwrap_or("");
                if !matches!(name, "target" | "node_modules" | ".git") {
                    stack.push(p);
                }
            } else if let Some(s) = p.to_str() {
                out.push(s.to_string());
            }
        }
    }
    out
}

/// CLI entry point for `truth claims`.
pub fn claims(paths: &[String], out: Option<&str>, json: bool) -> Result<()> {
    let config = load_config()?;
    let specs = extract_claims(paths, &config)?;

    if specs.is_empty() {
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({"claims": []}))?
            );
        } else {
            println!("No checkable claims found.\n");
            println!("Tip:");
            println!("truth claims README.md docs/");
        }
        return Ok(());
    }

    let claim_file = ClaimFile {
        version: 1,
        metadata: ClaimFileMetadata {
            name: Some("extracted claims".to_string()),
            description: Some("Generated by truth claims".to_string()),
        },
        defaults: ClaimDefaults {
            repo: Some(".".to_string()),
            window: Some("7d".to_string()),
            ..Default::default()
        },
        claims: specs,
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&claim_file)?);
    } else if let Some(path) = out {
        let yaml = claim_file.to_yaml().context("serializing claim file")?;
        std::fs::write(path, yaml).with_context(|| format!("writing {path}"))?;
        println!("Wrote {} claim(s) to {path}", claim_file.claims.len());
    } else {
        println!("{}", claim_file.to_yaml()?);
    }
    Ok(())
}
