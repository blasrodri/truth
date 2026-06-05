//! Repo file walker.
//!
//! By default `truth index .` walks the **entire** repo tree and indexes every
//! recognized text/code/config file, skipping excluded and always-noise
//! directories plus binary/oversized files. This is what makes indexing work on
//! real repos whose code does not live under a literal `src/` directory.
//!
//! If the user sets `[repo] include` to something other than the built-in
//! default, those entries act as path scopes (only files under them are
//! indexed) — so scoping is still possible when wanted.

use std::path::{Path, PathBuf};
use truth_core::config::RepoConfig;

/// Directories always skipped regardless of config (VCS, build output, deps).
const ALWAYS_SKIP_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "target",
    "node_modules",
    "dist",
    "build",
    ".next",
    ".venv",
    "venv",
    "__pycache__",
    ".mypy_cache",
    ".pytest_cache",
    "vendor",
    ".idea",
    ".vscode",
];

/// Extensions we attempt to index. Anything else (images, binaries, lockfiles
/// of unknown type) is skipped cheaply by extension before any read.
const INDEXABLE_EXTS: &[&str] = &[
    // code
    "rs", "ts", "tsx", "js", "jsx", "py", "go", "rb", "java", "kt", "kts", "scala", "cs", "php",
    "c", "h", "cc", "cpp", "hpp", "swift", "m", "mm", "ex", "exs", "erl", "clj", "sh", "bash",
    // config / data / docs
    "toml", "yaml", "yml", "json", "json5", "ini", "conf", "cfg", "env", "properties", "xml",
    "md", "markdown", "rst", "adoc", "txt", "sql", "proto", "graphql", "gradle",
];

/// Filenames (no/various extension) that are worth indexing.
const INDEXABLE_FILENAMES: &[&str] = &[
    "README",
    "Dockerfile",
    "Makefile",
    "go.mod",
    "go.sum",
    "requirements.txt",
    ".env",
    ".env.example",
    "docker-compose.yml",
    "docker-compose.yaml",
];

/// Files larger than this are skipped (generated blobs, minified bundles, etc.).
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024; // 2 MiB

/// The built-in default include list. When `include` equals this, we treat it
/// as "scan the whole tree" rather than a literal path allowlist.
fn is_default_include(include: &[String]) -> bool {
    let default = RepoConfig::default().include;
    include == default.as_slice()
}

/// A file selected for indexing, with cheap change-detection metadata captured
/// during the walk (no extra syscall — `ignore` already stat'd the entry).
pub struct WalkedFile {
    pub path: PathBuf,
    /// Modified time as unix epoch seconds (0 if unavailable).
    pub mtime: i64,
    /// File size in bytes.
    pub size: u64,
}

/// Resolve the set of files to index under `root`.
pub fn walk(root: &Path, cfg: &RepoConfig) -> Vec<WalkedFile> {
    // Custom, non-default include list → use it as path scopes (legacy behavior,
    // but still extension-filtered and binary-skipped).
    let scopes: Vec<PathBuf> = if cfg.include.is_empty() || is_default_include(&cfg.include) {
        vec![root.to_path_buf()]
    } else {
        cfg.include.iter().map(|i| root.join(i)).collect()
    };

    let out = std::sync::Mutex::new(Vec::new());

    // Build a parallel, .gitignore-aware walker rooted at the first scope, then
    // add the remaining scopes. `ignore` prunes git-ignored paths and walks on
    // all cores (this is what powers ripgrep).
    let mut scopes_iter = scopes.iter();
    let first = match scopes_iter.next() {
        Some(s) => s,
        None => return Vec::new(),
    };
    let mut builder = ignore::WalkBuilder::new(first);
    for s in scopes_iter {
        builder.add(s);
    }
    builder
        .standard_filters(true) // .gitignore, .ignore, hidden files
        .git_global(false)
        .require_git(false)
        .filter_entry({
            let root = root.to_path_buf();
            let cfg_exclude = cfg.exclude.clone();
            move |e| {
                // Prune always-skip and configured-exclude directories early.
                if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    let name = e.file_name().to_string_lossy();
                    if ALWAYS_SKIP_DIRS.contains(&name.as_ref())
                        || cfg_exclude.iter().any(|x| x == &name)
                    {
                        return false;
                    }
                    // Also prune if a parent component is excluded.
                    return !is_excluded_with(e.path(), &root, &cfg_exclude);
                }
                true
            }
        });

    builder.build_parallel().run(|| {
        let out = &out;
        let root = root.to_path_buf();
        let cfg_exclude = cfg.exclude.clone();
        Box::new(move |result| {
            if let Ok(entry) = result {
                let p = entry.path();
                if entry.file_type().map(|t| t.is_file()).unwrap_or(false)
                    && !is_excluded_with(p, &root, &cfg_exclude)
                    && is_indexable_file(p)
                {
                    // `ignore` already stat'd the entry during the walk, so
                    // reading metadata here is effectively free.
                    let md = entry.metadata().ok();
                    let size = md.as_ref().map(|m| m.len()).unwrap_or(0);
                    if size > MAX_FILE_BYTES {
                        return ignore::WalkState::Continue;
                    }
                    // Nanosecond-precision mtime: same-second edits must still be
                    // detected, so we cannot truncate to whole seconds.
                    let mtime = md
                        .and_then(|m| m.modified().ok())
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_nanos() as i64)
                        .unwrap_or(0);
                    out.lock().unwrap().push(WalkedFile {
                        path: p.to_path_buf(),
                        mtime,
                        size,
                    });
                }
            }
            ignore::WalkState::Continue
        })
    });

    let mut out = out.into_inner().unwrap();
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out.dedup_by(|a, b| a.path == b.path);
    out
}

/// Like `is_excluded`, but takes the exclude list directly (for closures that
/// can't borrow the whole config).
fn is_excluded_with(path: &Path, root: &Path, exclude: &[String]) -> bool {
    let rel = path.strip_prefix(root).unwrap_or(path);
    rel.components().any(|c| {
        let s = c.as_os_str().to_string_lossy();
        exclude.iter().any(|e| e == &s) || ALWAYS_SKIP_DIRS.contains(&s.as_ref())
    })
}

/// Whether a file is worth attempting to index (by extension or known name).
fn is_indexable_file(path: &Path) -> bool {
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        let ext = ext.to_ascii_lowercase();
        if INDEXABLE_EXTS.contains(&ext.as_str()) {
            return true;
        }
    }
    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        // Match exact known filenames, and any dotfile env variant.
        if INDEXABLE_FILENAMES.contains(&name) || name.starts_with(".env") {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn touch(p: &Path) {
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(p, "x = 1\n").unwrap();
    }

    fn default_cfg() -> RepoConfig {
        RepoConfig::default()
    }

    #[test]
    fn indexes_whole_tree_by_default_not_just_src() {
        let dir = std::env::temp_dir().join(format!("truth_walk_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        // Code outside src/ — the real-world case that the old walker missed.
        touch(&dir.join("crates/foo/src/lib.rs"));
        touch(&dir.join("cmd/server/main.go"));
        touch(&dir.join("app/handlers.py"));
        touch(&dir.join("README.md"));
        // Noise that must be skipped.
        touch(&dir.join("target/debug/junk.rs"));
        touch(&dir.join("node_modules/pkg/index.js"));
        touch(&dir.join("logo.png"));

        let files = walk(&dir, &default_cfg());
        let names: Vec<String> = files.iter().map(|p| p.path.strip_prefix(&dir).unwrap().to_string_lossy().into_owned()).collect();

        assert!(names.iter().any(|n| n.ends_with("crates/foo/src/lib.rs")));
        assert!(names.iter().any(|n| n.ends_with("cmd/server/main.go")));
        assert!(names.iter().any(|n| n.ends_with("app/handlers.py")));
        assert!(names.iter().any(|n| n == "README.md"));
        // Skipped:
        assert!(!names.iter().any(|n| n.contains("target/")));
        assert!(!names.iter().any(|n| n.contains("node_modules/")));
        assert!(!names.iter().any(|n| n.ends_with(".png")));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn custom_include_scopes_to_those_paths() {
        let dir = std::env::temp_dir().join(format!("truth_walk_scope_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        touch(&dir.join("backend/app.py"));
        touch(&dir.join("frontend/app.ts"));

        let mut cfg = default_cfg();
        cfg.include = vec!["backend".into()]; // non-default → scope

        let files = walk(&dir, &cfg);
        let names: Vec<String> = files.iter().map(|p| p.path.strip_prefix(&dir).unwrap().to_string_lossy().into_owned()).collect();
        assert!(names.iter().any(|n| n.ends_with("backend/app.py")));
        assert!(!names.iter().any(|n| n.ends_with("frontend/app.ts")));

        let _ = fs::remove_dir_all(&dir);
    }
}
