//! Owners resolution test: build a tiny repo with a MAINTAINERS file + git
//! history, index it, and confirm `truth owners` reports the declared owner.

use std::process::Command;
use truth_cli::owners::build_report;

fn sh(dir: &std::path::Path, args: &[&str]) {
    let ok = Command::new(args[0])
        .args(&args[1..])
        .current_dir(dir)
        .status()
        .unwrap()
        .success();
    assert!(ok, "command failed: {args:?}");
}

#[test]
fn owners_reports_declared_maintainer() {
    let dir = std::env::temp_dir().join(format!("truth_owners_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();

    std::fs::write(
        dir.join("MAINTAINERS"),
        "PAYMENTS\nM:\tAlice Pay <alice@example.com>\nR:\tBob Review <bob@example.com>\nF:\tsrc/\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src/routes.rs"),
        "fn r(r:&mut R){ r.post(\"/v1/checkout\", h); }\n",
    )
    .unwrap();

    // Make it a real git repo with one commit.
    sh(&dir, &["git", "init", "-q"]);
    sh(&dir, &["git", "config", "user.email", "t@t.co"]);
    sh(&dir, &["git", "config", "user.name", "Alice Pay"]);
    sh(&dir, &["git", "add", "-A"]);
    sh(&dir, &["git", "commit", "-q", "-m", "init"]);

    // Index into an in-memory DB.
    let conn = truth_db::open_in_memory().unwrap();
    let config = truth_core::config::Config::from_toml_str("").unwrap();
    truth_indexer::index_repo(&conn, &config.repo, Some(&dir)).unwrap();

    // Resolve owners for the route's file (absolute path resolves directly).
    let file = dir.join("src/routes.rs");
    let report = build_report(&conn, &file.to_string_lossy()).unwrap();

    assert!(
        report
            .owners
            .iter()
            .any(|o| o.who.contains("Alice Pay") && o.kind == "maintainer"),
        "owners: {:?}",
        report.owners
    );
    assert!(report
        .owners
        .iter()
        .any(|o| o.who.contains("Bob Review") && o.kind == "reviewer"));
    // The committer (also Alice) appears as a git signal.
    assert!(report.owners.iter().any(|o| o.kind == "recent_committer"));

    let _ = std::fs::remove_dir_all(&dir);
}
