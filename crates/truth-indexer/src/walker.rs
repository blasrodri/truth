//! Repo file walker honoring `[repo] include/exclude` (spec §10).

use std::path::{Path, PathBuf};
use truth_core::config::RepoConfig;
use walkdir::WalkDir;

/// Resolve the set of files to index under `root`, applying include/exclude.
pub fn walk(root: &Path, cfg: &RepoConfig) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for inc in &cfg.include {
        let target = root.join(inc);
        if target.is_file() {
            if !is_excluded(&target, root, cfg) {
                out.push(target);
            }
            continue;
        }
        if target.is_dir() {
            for entry in WalkDir::new(&target).into_iter().filter_map(Result::ok) {
                let p = entry.path();
                if p.is_file() && !is_excluded(p, root, cfg) {
                    out.push(p.to_path_buf());
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

fn is_excluded(path: &Path, root: &Path, cfg: &RepoConfig) -> bool {
    let rel = path.strip_prefix(root).unwrap_or(path);
    rel.components().any(|c| {
        let s = c.as_os_str().to_string_lossy();
        cfg.exclude.iter().any(|e| e == &s)
    })
}
