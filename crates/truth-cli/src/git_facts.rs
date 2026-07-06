//! Git-state evidence for `GitState` claims ("committed as <sha>", "pushed to
//! origin main", "on branch X", "no longer ahead"). Everything here reads the
//! LOCAL object store and remote-tracking refs — deterministic, no network.
//! `git push` updates the local tracking ref, so containment in a remote ref
//! is sound evidence a push happened from this clone; the verdict caveats the
//! push-from-elsewhere edge.
//!
//! Field audit: 26 true git-state claims were refused because nothing
//! consulted git for them.

use std::process::Command;
use truth_core::enums::{Authority, EvidenceType, ExtractionMethod};
use truth_core::models::EvidenceItem;
use truth_core::new_id;

fn git_stdout(repo_dir: &str, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo_dir)
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Words in a push-target phrase that never name a remote or branch
/// ("pushed the branch to the blasrodri fork" → token `blasrodri`).
fn is_target_stopword(w: &str) -> bool {
    matches!(
        w,
        "the" | "a" | "an" | "fork" | "remote" | "repo" | "repository" | "branch" | "and"
    )
}

/// Whether any remote-tracking ref containing `rev` matches the `target`
/// phrase (every non-stopword token appears in the ref path). Empty target →
/// any containing ref counts. Returns (matched, first matching ref, whether
/// any remote refs exist at all).
fn pushed_state(repo_dir: &str, rev: &str, target: &str) -> (Option<bool>, Option<String>) {
    // No remote refs at all → unverifiable, not false.
    let all_refs = git_stdout(repo_dir, &["branch", "-r", "--format=%(refname:short)"]);
    match &all_refs {
        Some(s) if !s.is_empty() => {}
        _ => return (None, None),
    }
    let containing = git_stdout(
        repo_dir,
        &[
            "branch",
            "-r",
            "--contains",
            rev,
            "--format=%(refname:short)",
        ],
    )
    .unwrap_or_default();
    let tokens: Vec<String> = target
        .split(|c: char| c.is_whitespace() || c == '/')
        .map(|t| t.trim().to_ascii_lowercase())
        .filter(|t| !t.is_empty() && !is_target_stopword(t))
        .collect();
    let hit = containing.lines().map(str::trim).find(|r| {
        let rl = r.to_ascii_lowercase();
        tokens
            .iter()
            .all(|t| rl.split('/').any(|seg| seg == *t) || rl.contains(t.as_str()))
    });
    (Some(hit.is_some()), hit.map(str::to_string))
}

/// Gather the `git_state` evidence item for the asserted parts. `None` when
/// this isn't a git repo with a HEAD — "git can't see it" is not evidence.
pub fn gather(repo_dir: &str, asserted: &serde_json::Value) -> Option<(EvidenceItem, Vec<String>)> {
    git_stdout(repo_dir, &["rev-parse", "--git-dir"])?;
    let mut meta = serde_json::Map::new();
    let mut lines = Vec::new();

    let sha = asserted.get("sha").and_then(|v| v.as_str());
    if let Some(sha) = sha {
        let exists = git_stdout(repo_dir, &["cat-file", "-t", sha])
            .map(|t| t == "commit")
            .unwrap_or(false);
        meta.insert("sha_exists".into(), exists.into());
        lines.push(if exists {
            format!("git: commit `{sha}` exists locally")
        } else {
            format!("git: no commit `{sha}` in this repository")
        });
    }

    if let Some(target) = asserted.get("pushed_to").and_then(|v| v.as_str()) {
        let rev = sha.unwrap_or("HEAD");
        let (pushed, remote_ref) = pushed_state(repo_dir, rev, target);
        match (pushed, &remote_ref) {
            (Some(true), Some(r)) => {
                meta.insert("pushed".into(), true.into());
                meta.insert("remote_ref".into(), r.clone().into());
                lines.push(format!("git: `{rev}` is contained in `{r}`"));
            }
            (Some(false), _) => {
                meta.insert("pushed".into(), false.into());
                lines.push(format!(
                    "git: no remote-tracking ref{} contains `{rev}`",
                    if target.is_empty() {
                        String::new()
                    } else {
                        format!(" matching `{target}`")
                    }
                ));
            }
            _ => {
                lines.push("git: no remote-tracking refs to check the push against".to_string());
            }
        }
    }

    if let Some(branch) = asserted.get("branch").and_then(|v| v.as_str()) {
        let exists = git_stdout(
            repo_dir,
            &["rev-parse", "--verify", &format!("refs/heads/{branch}")],
        )
        .is_some();
        meta.insert("branch_exists".into(), exists.into());
        let ok = exists
            && match sha {
                Some(sha) => git_stdout(
                    repo_dir,
                    &[
                        "branch",
                        "--contains",
                        sha,
                        "--list",
                        branch,
                        "--format=%(refname:short)",
                    ],
                )
                .map(|s| !s.is_empty())
                .unwrap_or(false),
                None => true,
            };
        meta.insert("branch_ok".into(), ok.into());
        lines.push(if ok {
            format!(
                "git: branch `{branch}` exists{}",
                if sha.is_some() {
                    " and contains the commit"
                } else {
                    ""
                }
            )
        } else if exists {
            format!("git: branch `{branch}` does not contain the named commit")
        } else {
            format!("git: no branch `{branch}` in this repository")
        });
    }

    if asserted.get("ahead_zero").and_then(|v| v.as_bool()) == Some(true) {
        if let Some(n) = git_stdout(repo_dir, &["rev-list", "--count", "@{upstream}..HEAD"])
            .and_then(|s| s.parse::<i64>().ok())
        {
            meta.insert("ahead".into(), n.into());
            lines.push(format!("git: HEAD is {n} commit(s) ahead of upstream"));
        } else {
            lines.push("git: no upstream configured — ahead/behind unknown".to_string());
        }
    }

    let item = EvidenceItem {
        id: new_id(),
        span_id: String::new(),
        evidence_type: EvidenceType::Change,
        subject_text: sha.map(str::to_string),
        subject_concept_id: None,
        predicate: Some("git_state".into()),
        object_text: None,
        value_json: Some(true.into()),
        unit: None,
        confidence: 0.9,
        authority: Authority::Code,
        valid_from: None,
        valid_to: None,
        extraction_method: ExtractionMethod::Deterministic,
        metadata_json: serde_json::Value::Object(meta),
    };
    Some((item, lines))
}
