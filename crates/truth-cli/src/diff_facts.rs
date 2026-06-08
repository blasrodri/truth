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

/// Scan the working-tree diff (staged + unstaged vs `HEAD`) in `repo_dir` for
/// `subject`. Returns `Untouched` if git is unavailable or the subject doesn't
/// appear — the caller then falls back to the index, so this never blocks.
pub fn scan(repo_dir: &str, subject: &str) -> Result<DiffFact> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo_dir)
        .args(["diff", "HEAD", "--unified=0", "--no-color"])
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
        assert_eq!(f.as_existence_item().unwrap().value_json, Some(json!(false)));
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
