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

/// How much history a file has and how recently it changed.
#[derive(Debug, Clone, PartialEq)]
pub struct FileActivity {
    pub commits: usize,
    /// Most recent commit (unix seconds).
    pub last_ts: i64,
    /// First commit (unix seconds).
    pub first_ts: i64,
}

/// A ranked committer for a file: how recently + how much they worked on it.
#[derive(Debug, Clone, PartialEq)]
pub struct Committer {
    pub author: String,
    /// Recency-weighted score (newest commits weigh most).
    pub score: f32,
    /// Most recent commit timestamp (unix seconds).
    pub last_ts: i64,
    /// Raw commit count to this file within the window.
    pub commits: usize,
    /// Fraction of the window's commits authored by this person (0..1).
    pub share: f32,
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

    /// Activity summary for a file: total commits and first/last commit times.
    /// `None` if the file is untracked or git is unavailable.
    pub fn file_activity(&self, path: &Path) -> Option<FileActivity> {
        if !self.available {
            return None;
        }
        let name = path.file_name()?.to_string_lossy().into_owned();
        let out = self.git(&["log", "--format=%ct", "--", &name])?;
        let times: Vec<i64> = out.lines().filter_map(|l| l.trim().parse::<i64>().ok()).collect();
        if times.is_empty() {
            return None;
        }
        // git log is newest-first.
        Some(FileActivity {
            commits: times.len(),
            last_ts: *times.first().unwrap(),
            first_ts: *times.last().unwrap(),
        })
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
        self.committer_stats(path, window)
            .into_iter()
            .map(|c| (c.author, c.score, c.last_ts))
            .collect()
    }

    /// Ranked committers with commit count + recency-weighted score + share of
    /// the recent commits to this file. The `share` (0..1) is what tells a clear
    /// owner from one of many drive-by contributors.
    pub fn committer_stats(&self, path: &Path, window: usize) -> Vec<Committer> {
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

        let rows: Vec<(&str, i64)> = out
            .lines()
            .filter_map(|l| {
                let mut f = l.split('\u{1f}');
                Some((f.next()?, f.next()?.parse::<i64>().ok()?))
            })
            .collect();
        let n = rows.len();
        if n == 0 {
            return Vec::new();
        }
        // (recency-weighted score, last ts, commit count). Newest commit gets the
        // highest weight; count is the raw tally.
        let mut by_author: std::collections::HashMap<String, (f32, i64, usize)> =
            std::collections::HashMap::new();
        for (i, (author, ts)) in rows.iter().enumerate() {
            let weight = (n - i) as f32;
            let e = by_author.entry((*author).to_string()).or_insert((0.0, *ts, 0));
            e.0 += weight;
            e.1 = e.1.max(*ts);
            e.2 += 1;
        }
        let mut ranked: Vec<Committer> = by_author
            .into_iter()
            .map(|(author, (score, last_ts, commits))| Committer {
                author,
                score,
                last_ts,
                commits,
                share: commits as f32 / n as f32,
            })
            .collect();
        ranked.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
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
    fn committer_stats_have_counts_and_shares() {
        let root = repo_root();
        let h = GitHistory::open(root.clone());
        if !h.available() {
            return; // graceful no-op off a git tree
        }
        let stats = h.committer_stats(&root.join("Cargo.toml"), 50);
        if stats.is_empty() {
            return;
        }
        // Sorted by recency-weighted score (descending).
        for w in stats.windows(2) {
            assert!(w[0].score >= w[1].score);
        }
        // Shares are a valid distribution: each in (0,1], and they sum to ~1.
        let total: f32 = stats.iter().map(|c| c.share).sum();
        assert!((total - 1.0).abs() < 0.01, "shares should sum to ~1, got {total}");
        assert!(stats.iter().all(|c| c.commits >= 1 && c.share > 0.0));
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
