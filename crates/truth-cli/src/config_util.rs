//! Shared config/JSON helpers used across CLI command modules.

use anyhow::Result;
use std::path::{Path, PathBuf};
use truth_core::config::Config;

/// Walk up from `start` to the nearest directory containing a truth root
/// marker (`truth.toml` or `.truth/`) — the same discovery git does for
/// `.git`. Without this, indexing in one directory and querying from another
/// silently returned empty results.
pub fn discover_root_from(start: &Path) -> Option<PathBuf> {
    let mut dir = start.to_path_buf();
    loop {
        if dir.join("truth.toml").is_file() || dir.join(".truth").is_dir() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Discover the truth root for the current process working directory.
pub fn discover_root() -> Option<PathBuf> {
    discover_root_from(&std::env::current_dir().ok()?)
}

/// Load `truth.toml` from the nearest truth root (walking up from the current
/// directory), anchoring relative paths (repo root, database path) at that
/// root so every command works from any subdirectory. Falls back to built-in
/// defaults when no root exists anywhere up the tree.
pub fn load_config() -> Result<Config> {
    let Some(root) = discover_root() else {
        return Config::from_toml_str("");
    };
    let toml = root.join("truth.toml");
    let mut config = if toml.is_file() {
        Config::load(&toml)?
    } else {
        Config::from_toml_str("")?
    };
    anchor_at(&mut config, &root);
    Ok(config)
}

/// Re-anchor the config's relative paths at `root` instead of the process CWD.
pub fn anchor_at(config: &mut Config, root: &Path) {
    let rootify = |p: &str| -> String {
        let path = Path::new(p);
        if path.is_absolute() {
            p.to_string()
        } else if p == "." {
            root.to_string_lossy().into_owned()
        } else {
            root.join(path).to_string_lossy().into_owned()
        }
    };
    config.repo.root = rootify(&config.repo.root);
    config.database.path = rootify(&config.database.path);
}

/// Pretty-print a JSON value to stdout.
pub fn print_json(v: &serde_json::Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(v).expect("json serializes")
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_root_from_nested_subdir() {
        let tmp = std::env::temp_dir().join(format!("truth-root-{}", std::process::id()));
        let nested = tmp.join("a/b/c");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(tmp.join(".truth")).unwrap();

        let found = discover_root_from(&nested).unwrap();
        assert_eq!(found, tmp);

        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn no_marker_anywhere_yields_none() {
        // Filesystem root has no truth.toml/.truth (if it does, the host is
        // already a truth repo and this assertion is the least of our worries).
        assert!(discover_root_from(Path::new("/nonexistent-dir-xyz")).is_none());
    }

    #[test]
    fn anchor_rewrites_relative_paths_only() {
        let mut cfg = Config::default();
        anchor_at(&mut cfg, Path::new("/work/proj"));
        assert_eq!(cfg.repo.root, "/work/proj");
        assert_eq!(cfg.database.path, "/work/proj/.truth/truth.sqlite");

        let mut abs = Config::default();
        abs.database.path = "/elsewhere/db.sqlite".into();
        anchor_at(&mut abs, Path::new("/work/proj"));
        assert_eq!(abs.database.path, "/elsewhere/db.sqlite");
    }
}
