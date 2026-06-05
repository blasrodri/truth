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
use walkdir::WalkDir;

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

/// Resolve the set of files to index under `root`.
pub fn walk(root: &Path, cfg: &RepoConfig) -> Vec<PathBuf> {
    // Custom, non-default include list → use it as path scopes (legacy behavior,
    // but still extension-filtered and binary-skipped).
    let scopes: Vec<PathBuf> = if cfg.include.is_empty() || is_default_include(&cfg.include) {
        vec![root.to_path_buf()]
    } else {
        cfg.include.iter().map(|i| root.join(i)).collect()
    };

    let mut out = Vec::new();
    for scope in &scopes {
        if scope.is_file() {
            if !is_excluded(scope, root, cfg) && is_indexable_file(scope) {
                out.push(scope.clone());
            }
            continue;
        }
        let walker = WalkDir::new(scope).into_iter().filter_entry(|e| {
            // Prune skipped directories so we never descend into them.
            !(e.file_type().is_dir() && is_skipped_dir(e.path(), root, cfg))
        });
        for entry in walker.filter_map(Result::ok) {
            let p = entry.path();
            if entry.file_type().is_file()
                && !is_excluded(p, root, cfg)
                && is_indexable_file(p)
                && !is_too_large(p)
            {
                out.push(p.to_path_buf());
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// A directory is skipped if it is an always-skip dir or matches an exclude.
fn is_skipped_dir(path: &Path, root: &Path, cfg: &RepoConfig) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if ALWAYS_SKIP_DIRS.contains(&name) {
        return true;
    }
    is_excluded(path, root, cfg)
}

/// A path is excluded if any of its components matches a configured exclude.
fn is_excluded(path: &Path, root: &Path, cfg: &RepoConfig) -> bool {
    let rel = path.strip_prefix(root).unwrap_or(path);
    rel.components().any(|c| {
        let s = c.as_os_str().to_string_lossy();
        cfg.exclude.iter().any(|e| e == &s) || ALWAYS_SKIP_DIRS.contains(&s.as_ref())
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

fn is_too_large(path: &Path) -> bool {
    std::fs::metadata(path).map(|m| m.len() > MAX_FILE_BYTES).unwrap_or(false)
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
        let names: Vec<String> = files.iter().map(|p| p.strip_prefix(&dir).unwrap().to_string_lossy().into_owned()).collect();

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
        let names: Vec<String> = files.iter().map(|p| p.strip_prefix(&dir).unwrap().to_string_lossy().into_owned()).collect();
        assert!(names.iter().any(|n| n.ends_with("backend/app.py")));
        assert!(!names.iter().any(|n| n.ends_with("frontend/app.ts")));

        let _ = fs::remove_dir_all(&dir);
    }
}
