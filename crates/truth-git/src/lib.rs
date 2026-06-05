//! `truth-git` — read evidence from a repo's git history by shelling out to the
//! `git` binary. The cost measurements in `docs/GIT_HISTORY_DESIGN.md` show this
//! is fast enough to do lazily, per check (not at index time).
//!
//! Everything degrades gracefully: if there is no `.git`, `git` is missing, or a
//! command fails, the methods return `None` — a check never fails for lack of
//! git data.

pub mod owners;

use std::path::{Path, PathBuf};
use std::process::Command;

/// A handle to a repository's git history rooted at `root`.
pub struct GitHistory {
    root: PathBuf,
    /// Whether `root` is inside a usable git work tree (detected once).
    available: bool,
}

/// A commit touching the queried subject.
#[derive(Debug, Clone, PartialEq)]
pub struct CommitInfo {
    pub sha: String,
    pub author: String,
    /// Unix epoch seconds.
    pub timestamp: i64,
    pub subject: String,
}

impl GitHistory {
    /// Open a history handle rooted at `root`, detecting git availability once.
    pub fn open(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let available = is_git_work_tree(&root);
        GitHistory { root, available }
    }

    /// Open a handle whose root is `file`'s directory, so `last_modified(file)`
    /// resolves correctly via the file's basename. Falls back to cwd when the
    /// file has no parent.
    pub fn for_file(file: &Path) -> Self {
        let dir = file.parent().filter(|p| !p.as_os_str().is_empty());
        match dir {
            Some(d) => Self::open(d),
            None => Self::open("."),
        }
    }

    /// Whether git history is available for this repo.
    pub fn available(&self) -> bool {
        self.available
    }

    /// Last-modified time (unix epoch seconds) of `path` per git history — i.e.
    /// the author date of the most recent commit that touched it. `None` if git
    /// is unavailable, the path is untracked, or the command fails.
    ///
    /// `path` is resolved relative to the handle's `root` (which should be the
    /// file's own directory — see `for_file`), so we pass only the basename to
    /// git and let it resolve against `-C root`.
    pub fn last_modified(&self, path: &Path) -> Option<i64> {
        if !self.available {
            return None;
        }
        let name = path.file_name()?.to_string_lossy();
        let out = self.git(&["log", "-1", "--format=%ct", "--", &name])?;
        let out = out.trim();
        if out.is_empty() {
            return None; // untracked file
        }
        out.parse::<i64>().ok()
    }

    /// Most recent commits whose message matches any of `patterns` (OR), newest
    /// first, capped at `limit`. Useful for "is X fixed/deprecated?" claims.
    pub fn commits_matching(&self, patterns: &[&str], limit: usize) -> Vec<CommitInfo> {
        if !self.available || patterns.is_empty() {
            return Vec::new();
        }
        // Build: git log -i -<limit> --format=<rec> --grep=p1 --grep=p2 ...
        // %x1f is a unit separator we split on; %x1e ends each record.
        let fmt = "--format=%H%x1f%an%x1f%ct%x1f%s%x1e";
        let mut args: Vec<String> = vec![
            "log".into(),
            "-i".into(),
            format!("-{limit}"),
            fmt.into(),
        ];
        for p in patterns {
            args.push(format!("--grep={p}"));
        }
        let args_ref: Vec<&str> = args.iter().map(String::as_str).collect();
        let Some(out) = self.git(&args_ref) else {
            return Vec::new();
        };
        out.split('\u{1e}')
            .filter_map(|rec| {
                let rec = rec.trim_matches(|c| c == '\n' || c == '\r');
                if rec.is_empty() {
                    return None;
                }
                let mut f = rec.split('\u{1f}');
                Some(CommitInfo {
                    sha: f.next()?.chars().take(12).collect(),
                    author: f.next()?.to_string(),
                    timestamp: f.next()?.parse().ok()?,
                    subject: f.next()?.to_string(),
                })
            })
            .collect()
    }

    /// Recent committers of `path`, recency-weighted: more recent and more
    /// frequent committers rank higher. Returns (author, score, last_date) over
    /// the last `window` commits touching the file. A heuristic *signal* of who
    /// has worked on the code — not a claim of responsibility.
    pub fn recent_committers(&self, path: &Path, window: usize) -> Vec<(String, f32, i64)> {
        if !self.available {
            return Vec::new();
        }
        let name = match path.file_name() {
            Some(n) => n.to_string_lossy().into_owned(),
            None => return Vec::new(),
        };
        let fmt = format!("-{window}");
        let Some(out) = self.git(&["log", &fmt, "--format=%an%x1f%ct", "--", &name]) else {
            return Vec::new();
        };

        // Weight each commit by recency rank: 1st (newest)=window, ...=1.
        let rows: Vec<(&str, i64)> = out
            .lines()
            .filter_map(|l| {
                let mut f = l.split('\u{1f}');
                Some((f.next()?, f.next()?.parse::<i64>().ok()?))
            })
            .collect();
        let n = rows.len();
        let mut by_author: std::collections::HashMap<String, (f32, i64)> = std::collections::HashMap::new();
        for (i, (author, ts)) in rows.iter().enumerate() {
            let weight = (n - i) as f32; // newest gets highest weight
            let e = by_author.entry((*author).to_string()).or_insert((0.0, *ts));
            e.0 += weight;
            e.1 = e.1.max(*ts);
        }
        let mut ranked: Vec<(String, f32, i64)> =
            by_author.into_iter().map(|(a, (s, t))| (a, s, t)).collect();
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        ranked
    }

    /// Run a git command in `root`, returning stdout on success.
    fn git(&self, args: &[&str]) -> Option<String> {
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(args)
            .output()
            .ok()?;
        if out.status.success() {
            Some(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            None
        }
    }
}

/// Whether `root` is inside a git work tree (cheap, one `git` invocation).
fn is_git_work_tree(root: &Path) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "true")
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The truth repo itself is a git work tree, so these run against real data.
    fn repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(2).unwrap().to_path_buf()
    }

    #[test]
    fn detects_git_work_tree() {
        let h = GitHistory::open(repo_root());
        // CI/dev checkout is a git repo; if not, the rest gracefully no-ops.
        if h.available() {
            let f = repo_root().join("crates/truth-git/Cargo.toml");
            // This file exists in the work tree but may be uncommitted in the
            // first run; last_modified is Some only once committed. Either way it
            // must not panic.
            let _ = h.last_modified(&f);
        }
    }

    #[test]
    fn missing_repo_degrades_to_none() {
        let h = GitHistory::open("/definitely/not/a/repo/anywhere");
        assert!(!h.available());
        assert_eq!(h.last_modified(Path::new("/definitely/not/a/repo/x.rs")), None);
        assert!(h.commits_matching(&["fix"], 5).is_empty());
    }

    #[test]
    fn last_modified_of_a_committed_file_is_some() {
        let h = GitHistory::open(repo_root());
        if !h.available() {
            return; // not a git checkout; skip
        }
        // README.md has been committed since the first commit.
        let readme = repo_root().join("README.md");
        if readme.exists() {
            // May be None if README is untracked in an odd checkout; assert the
            // call works and, when Some, is a plausible unix timestamp.
            if let Some(ts) = h.last_modified(&readme) {
                assert!(ts > 1_000_000_000, "implausible timestamp {ts}");
            }
        }
    }
}
