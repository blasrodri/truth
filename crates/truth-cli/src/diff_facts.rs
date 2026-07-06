//! Git-diff evidence: "did THIS turn add / remove / change the subject?"
//!
//! The index tells you what the code *is now*; the diff tells you what *this
//! working tree changed* relative to `HEAD`. For agent-fact-checking, claims
//! like "I added `/v1/refund`" or "I removed `/v1/checkout`" are about the
//! change itself, so diff evidence is ranked ABOVE the index: it is the freshest
//! and most direct signal, and it works before any re-index.
//!
//! This adapter is intentionally deterministic and dependency-free: it shells
//! out to `git diff` and does literal +/- line matching for the subject. It
//! never invokes the LLM.

use anyhow::Result;
use serde_json::json;
use std::process::Command;
use truth_core::enums::{Authority, EvidenceType, ExtractionMethod};
use truth_core::models::EvidenceItem;
use truth_core::new_id;

/// What the working tree did to a subject relative to `HEAD`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffChange {
    /// Subject appears only in added (`+`) lines → introduced this turn.
    Added,
    /// Subject appears only in removed (`-`) lines → deleted this turn.
    Removed,
    /// Subject appears in both (e.g. a value changed on the line).
    Modified,
    /// Subject is untouched by the diff.
    Untouched,
}

/// Result of scanning the diff for a subject.
#[derive(Debug, Clone)]
pub struct DiffFact {
    pub subject: String,
    pub change: DiffChange,
    pub added_hits: usize,
    pub removed_hits: usize,
    /// First file the subject was touched in (for citation).
    pub file: Option<String>,
}

impl DiffFact {
    /// Whether the subject is present in the working tree *after* the change.
    /// Added/Modified leave it present; Removed makes it absent; Untouched is
    /// unknown from the diff alone (the index answers that).
    pub fn present_after(&self) -> Option<bool> {
        match self.change {
            DiffChange::Added | DiffChange::Modified => Some(true),
            DiffChange::Removed => Some(false),
            DiffChange::Untouched => None,
        }
    }

    /// Human-readable evidence line.
    pub fn evidence_line(&self) -> String {
        let where_ = self
            .file
            .as_deref()
            .map(|f| format!(" in {f}"))
            .unwrap_or_default();
        match self.change {
            DiffChange::Added => {
                format!("diff: `{}` was ADDED this turn{where_}", self.subject)
            }
            DiffChange::Removed => {
                format!("diff: `{}` was REMOVED this turn{where_}", self.subject)
            }
            DiffChange::Modified => {
                format!("diff: `{}` was MODIFIED this turn{where_}", self.subject)
            }
            DiffChange::Untouched => {
                format!("diff: `{}` is untouched by the working tree", self.subject)
            }
        }
    }

    /// A `route_exists`-predicate evidence item reflecting the post-change state,
    /// tagged `authored_by_diff` so it can be ranked above the index. Returns
    /// `None` when the diff says nothing about the subject's existence.
    pub fn as_existence_item(&self) -> Option<EvidenceItem> {
        let present = self.present_after()?;
        Some(EvidenceItem {
            id: new_id(),
            span_id: String::new(),
            evidence_type: EvidenceType::Change,
            subject_text: Some(self.subject.clone()),
            subject_concept_id: None,
            predicate: Some("route_exists".into()),
            object_text: None,
            value_json: Some(json!(present)),
            unit: None,
            confidence: 0.9,
            // Code authority: the diff IS the code change, freshest of all.
            authority: Authority::Code,
            valid_from: None,
            valid_to: None,
            extraction_method: ExtractionMethod::Deterministic,
            metadata_json: json!({
                "from_diff": true,
                "change": format!("{:?}", self.change),
                "file": self.file,
            }),
        })
    }
}

/// One file's change in the working-tree diff (`git diff HEAD --name-status`).
#[derive(Debug, Clone)]
pub struct FileChange {
    pub path: String,
    /// "added" | "modified" | "deleted" | "renamed"
    pub status: &'static str,
    /// For renames: the old path.
    pub renamed_from: Option<String>,
}

/// All files changed by the working tree relative to `HEAD` (staged +
/// unstaged, rename-aware), PLUS untracked files (`git diff HEAD` alone would
/// miss a brand-new file and falsely contradict "I created X"). Empty when
/// git is unavailable or the tree is clean — callers must treat "empty" as
/// "unknown", not "nothing changed", since the work may already be committed.
pub fn changed_files(repo_dir: &str) -> Vec<FileChange> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo_dir)
        .args(["diff", "HEAD", "--name-status", "-M", "--no-color"])
        .output();
    let text = match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        _ => return Vec::new(),
    };

    let mut changes = Vec::new();
    // Untracked (but not ignored) files are additions this turn.
    if let Ok(o) = Command::new("git")
        .arg("-C")
        .arg(repo_dir)
        .args(["ls-files", "--others", "--exclude-standard"])
        .output()
    {
        if o.status.success() {
            for path in String::from_utf8_lossy(&o.stdout).lines() {
                if !path.is_empty() {
                    changes.push(FileChange {
                        path: path.to_string(),
                        status: "added",
                        renamed_from: None,
                    });
                }
            }
        }
    }
    for line in text.lines() {
        let mut parts = line.split('\t');
        let Some(code) = parts.next() else { continue };
        let status = match code.chars().next() {
            Some('A') => "added",
            Some('M') => "modified",
            Some('D') => "deleted",
            Some('R') => "renamed",
            _ => continue,
        };
        if status == "renamed" {
            let (Some(from), Some(to)) = (parts.next(), parts.next()) else {
                continue;
            };
            changes.push(FileChange {
                path: to.to_string(),
                status,
                renamed_from: Some(from.to_string()),
            });
        } else if let Some(path) = parts.next() {
            changes.push(FileChange {
                path: path.to_string(),
                status,
                renamed_from: None,
            });
        }
    }
    changes
}

/// What HEAD knows about a file when the working tree is CLEAN — the fallback
/// evidence for "I edited/created X" claims made after the work was committed.
/// Field audit: an entire repo's worth of true claims (healthtrust360) refused
/// because verify was diff-only and everything was already committed.
#[derive(Debug, Clone)]
pub struct HeadFileFact {
    /// The tracked repo path resolved from the claim subject (suffix match
    /// first, then substring), if the file exists at HEAD.
    pub tracked_path: Option<String>,
    /// Whether that path was changed by the HEAD commit itself.
    pub in_head_commit: bool,
    pub head_sha: Option<String>,
    /// For deletion claims: whether the exact subject path appears in history
    /// even though it is not tracked now.
    pub ever_existed: bool,
}

fn git_lines(repo_dir: &str, args: &[&str]) -> Vec<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo_dir)
        .args(args)
        .output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

/// Resolve `subject` against a list of repo paths: exact, then suffix, then
/// substring — so "models.go" finds "internal/gk/models.go" without matching
/// "models.go.bak" first.
fn resolve_path<'a>(paths: &'a [String], subject: &str) -> Option<&'a String> {
    paths
        .iter()
        .find(|p| p.as_str() == subject)
        .or_else(|| {
            paths
                .iter()
                .find(|p| p.ends_with(subject) || subject.ends_with(p.as_str()))
        })
        .or_else(|| paths.iter().find(|p| p.contains(subject)))
}

/// Gather HEAD facts for `subject`. Cheap: three git commands, called only on
/// a clean tree for file-change claims. `None` when there is no HEAD to
/// consult (not a git repo, or no commits yet) — "git can't see the file" is
/// NOT evidence the file doesn't exist, and contradicting on it would be a
/// false accusation (the precision gate caught exactly that).
pub fn head_file_fact(repo_dir: &str, subject: &str) -> Option<HeadFileFact> {
    let head_sha = git_lines(repo_dir, &["rev-parse", "--short", "HEAD"])
        .into_iter()
        .next()?;
    let tracked = git_lines(repo_dir, &["ls-files"]);
    let tracked_path = resolve_path(&tracked, subject).cloned();
    let head_changed = git_lines(repo_dir, &["show", "--name-only", "--format=", "HEAD"]);
    let in_head_commit = tracked_path
        .as_deref()
        .map(|p| head_changed.iter().any(|h| h == p))
        .unwrap_or(false);
    let ever_existed = tracked_path.is_some()
        || !git_lines(repo_dir, &["log", "-1", "--format=%h", "--", subject]).is_empty();
    Some(HeadFileFact {
        tracked_path,
        in_head_commit,
        head_sha: Some(head_sha),
        ever_existed,
    })
}

/// Evidence item for the clean-tree HEAD fallback (`head_file_status`).
pub fn head_file_item(fact: &HeadFileFact, subject: &str) -> EvidenceItem {
    diff_evidence_item(
        "head_file_status",
        Some(subject.to_string()),
        json!(fact.tracked_path.is_some()),
        json!({
            "file": fact.tracked_path,
            "in_head_commit": fact.in_head_commit,
            "head_sha": fact.head_sha,
            "ever_existed": fact.ever_existed,
        }),
    )
}

/// Evidence item carrying the status of one changed file (`file_status`).
pub fn file_status_item(change: &FileChange) -> EvidenceItem {
    diff_evidence_item(
        "file_status",
        Some(change.path.clone()),
        json!(change.status),
        json!({
            "from_diff": true,
            "file": change.path,
            "renamed_from": change.renamed_from,
        }),
    )
}

/// Evidence item carrying the full diff file list (`diff_files`).
pub fn diff_files_item(changes: &[FileChange]) -> EvidenceItem {
    let paths: Vec<&str> = changes.iter().map(|c| c.path.as_str()).collect();
    diff_evidence_item(
        "diff_files",
        None,
        json!(paths),
        json!({ "from_diff": true, "count": changes.len() }),
    )
}

/// Evidence item carrying changed-line hit counts for a subject (`diff_hits`).
pub fn diff_hits_item(fact: &DiffFact) -> EvidenceItem {
    diff_evidence_item(
        "diff_hits",
        Some(fact.subject.clone()),
        json!(fact.added_hits),
        json!({
            "from_diff": true,
            "removed_hits": fact.removed_hits,
            "file": fact.file,
        }),
    )
}

/// Evidence item asserting whether a rename's NEW name is present after the
/// change (`renamed_to_exists`).
pub fn renamed_to_item(fact: &DiffFact) -> Option<EvidenceItem> {
    let present = fact.present_after()?;
    Some(diff_evidence_item(
        "renamed_to_exists",
        Some(fact.subject.clone()),
        json!(present),
        json!({ "from_diff": true, "file": fact.file }),
    ))
}

fn diff_evidence_item(
    predicate: &str,
    subject: Option<String>,
    value: serde_json::Value,
    metadata: serde_json::Value,
) -> EvidenceItem {
    EvidenceItem {
        id: new_id(),
        span_id: String::new(),
        evidence_type: EvidenceType::Change,
        subject_text: subject,
        subject_concept_id: None,
        predicate: Some(predicate.into()),
        object_text: None,
        value_json: Some(value),
        unit: None,
        confidence: 0.9,
        authority: Authority::Code,
        valid_from: None,
        valid_to: None,
        extraction_method: ExtractionMethod::Deterministic,
        metadata_json: metadata,
    }
}

/// Scan the working-tree diff (staged + unstaged vs `HEAD`) in `repo_dir` for
/// `subject`. Returns `Untouched` if git is unavailable or the subject doesn't
/// appear — the caller then falls back to the index, so this never blocks.
pub fn scan(repo_dir: &str, subject: &str) -> Result<DiffFact> {
    // `-- .` scopes the diff to files UNDER repo_dir. Without it, when repo_dir
    // is a subdirectory of a larger git repo (a vendored example, a monorepo
    // package), `git diff` reports the whole enclosing repo's changes — so a
    // subject string appearing in an unrelated sibling file would be read as
    // "added here". Scoping keeps the working-tree evidence local to the repo
    // the verifier was pointed at.
    let out = Command::new("git")
        .arg("-C")
        .arg(repo_dir)
        .args(["diff", "HEAD", "--unified=0", "--no-color", "--", "."])
        .output();

    let text = match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        // Not a git repo / git missing / no HEAD yet → treat as untouched.
        _ => {
            return Ok(DiffFact {
                subject: subject.to_string(),
                change: DiffChange::Untouched,
                added_hits: 0,
                removed_hits: 0,
                file: None,
            })
        }
    };

    let mut added = 0usize;
    let mut removed = 0usize;
    let mut file: Option<String> = None;
    let mut cur_file: Option<String> = None;

    for line in text.lines() {
        if let Some(path) = line.strip_prefix("+++ b/") {
            cur_file = Some(path.to_string());
            continue;
        }
        // Skip diff metadata lines that start with +/- but aren't content.
        if line.starts_with("+++") || line.starts_with("---") {
            continue;
        }
        let is_add = line.starts_with('+');
        let is_del = line.starts_with('-');
        if !is_add && !is_del {
            continue;
        }
        // Literal substring match for the subject on the changed line.
        if line.contains(subject) {
            if is_add {
                added += 1;
            } else {
                removed += 1;
            }
            if file.is_none() {
                file = cur_file.clone();
            }
        }
    }

    let change = match (added, removed) {
        (0, 0) => DiffChange::Untouched,
        (a, 0) if a > 0 => DiffChange::Added,
        (0, r) if r > 0 => DiffChange::Removed,
        _ => DiffChange::Modified,
    };

    Ok(DiffFact {
        subject: subject.to_string(),
        change,
        added_hits: added,
        removed_hits: removed,
        file,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_added_only() {
        let f = DiffFact {
            subject: "/v1/refund".into(),
            change: DiffChange::Added,
            added_hits: 1,
            removed_hits: 0,
            file: Some("src/routes.rs".into()),
        };
        assert_eq!(f.present_after(), Some(true));
        let item = f.as_existence_item().unwrap();
        assert_eq!(item.predicate.as_deref(), Some("route_exists"));
        assert_eq!(item.value_json, Some(json!(true)));
    }

    #[test]
    fn classifies_removed_only() {
        let f = DiffFact {
            subject: "/v1/checkout".into(),
            change: DiffChange::Removed,
            added_hits: 0,
            removed_hits: 2,
            file: None,
        };
        assert_eq!(f.present_after(), Some(false));
        assert_eq!(
            f.as_existence_item().unwrap().value_json,
            Some(json!(false))
        );
    }

    #[test]
    fn untouched_yields_no_item() {
        let f = DiffFact {
            subject: "/v1/x".into(),
            change: DiffChange::Untouched,
            added_hits: 0,
            removed_hits: 0,
            file: None,
        };
        assert!(f.as_existence_item().is_none());
    }

    #[test]
    fn scan_on_non_git_dir_is_untouched() {
        let f = scan("/nonexistent-dir-xyz", "/v1/checkout").unwrap();
        assert_eq!(f.change, DiffChange::Untouched);
    }
}
