//! The precision gate: truth's false-contradiction rate must be 0.
//!
//! `fixtures/precision/` is a labeled corpus of REAL agent over-claims (SWE-bench
//! n=46, auto-labeled against each agent's actual patch) plus the known-FP
//! English that false-contradicted truth on its own repo. The adoption-blocking
//! failure mode is NOT a missed catch — it's a FALSE ACCUSATION: truth telling a
//! truthful agent it lied. One of those and the Stop-hook gets uninstalled.
//!
//! Contract:
//!   narrative_prose.yaml  expected `inconclusive` — a flip to `contradicted` is
//!                         the gated false-contradiction (MUST be 0).
//!   true_claims.yaml      expected `supported`     — a flip to `contradicted` is
//!                         a false accusation on true work (MUST be 0).
//!   real_lies.yaml        expected `contradicted`  — recall, asserted only once
//!                         the per-instance repos are vendored (TODO markers).
//!
//! Recall (catching real lies) is reported by scripts/precision_gate.sh but is
//! deliberately NOT gated here: trust is earned by never crying wolf first.

use std::path::{Path, PathBuf};
use truth_cli::eval::{load_fixture, run_eval};
use truth_core::config::Config;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap()
        .to_path_buf()
}

fn offline() -> Config {
    let mut c = Config::from_toml_str("").unwrap();
    c.loki.enabled = false;
    c
}

/// Stage the committed empty fixture repo to a GIT-LESS temp dir. Narrative cases
/// reference `__EMPTY_REPO__`; pointed at a path inside the truth git repo, a
/// diff-native claim ("push.py was updated") would resolve against the parent
/// working tree and false-contradict. A git-less dir yields no diff → the verdict
/// engine correctly returns inconclusive. Returns the staged path; the dir is
/// unique per call so parallel tests don't collide.
fn stage_empty_repo() -> PathBuf {
    let src = repo_root().join("fixtures/precision/empty_repo");
    // Unique, git-less destination outside any repo.
    let dst = std::env::temp_dir().join(format!("truth_precision_empty_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dst);
    std::fs::create_dir_all(&dst).unwrap();
    for entry in std::fs::read_dir(&src).unwrap() {
        let entry = entry.unwrap();
        // Skip any nested .git so `git -C` finds no repo.
        if entry.file_name() == ".git" {
            continue;
        }
        let to = dst.join(entry.file_name());
        std::fs::copy(entry.path(), to).unwrap();
    }
    dst
}

/// Run a precision bucket and return the false-contradiction case names.
fn false_contradictions_in(fixture_name: &str, empty_repo: &Path) -> Vec<String> {
    let root = repo_root();
    let text = std::fs::read_to_string(root.join("fixtures/precision").join(fixture_name)).unwrap();
    // Substitute the empty-repo token before parsing.
    let text = text.replace("__EMPTY_REPO__", &empty_repo.to_string_lossy());
    let mut fixture = load_fixture(&text).unwrap();
    // Absolutize any other relative repos (recall buckets), like extractor_corpus.
    for case in &mut fixture.cases {
        if let Some(repo) = &case.repo {
            if !Path::new(repo).is_absolute() {
                case.repo = Some(root.join(repo).to_string_lossy().into_owned());
            }
        }
    }
    let report = run_eval(&offline(), &fixture).unwrap();
    report
        .false_contradictions()
        .iter()
        .map(|c| c.name.clone())
        .collect()
}

#[test]
fn precision_gate_zero_false_contradictions() {
    let empty = stage_empty_repo();

    let mut offenders = Vec::new();
    // Both buckets whose expected verdict is never `contradicted`.
    for bucket in ["narrative_prose.yaml", "true_claims.yaml"] {
        for name in false_contradictions_in(bucket, &empty) {
            offenders.push(format!("{bucket}::{name}"));
        }
    }

    let _ = std::fs::remove_dir_all(&empty);

    assert!(
        offenders.is_empty(),
        "PRECISION GATE FAILED — truth false-contradicted {} truthful claim(s):\n  {}\n\n\
         A false contradiction is the adoption blocker: truth told a truthful agent it lied. \
         Fix the extractor/verdict path so these refuse instead of contradicting.",
        offenders.len(),
        offenders.join("\n  "),
    );
}

/// Guard the corpus itself from silently shrinking (a deleted fixture would make
/// the gate vacuously pass).
#[test]
fn precision_corpus_is_populated() {
    let root = repo_root();
    let text =
        std::fs::read_to_string(root.join("fixtures/precision/narrative_prose.yaml")).unwrap();
    let text = text.replace("__EMPTY_REPO__", "/tmp/x");
    let fixture = load_fixture(&text).unwrap();
    assert!(
        fixture.cases.len() >= 40,
        "narrative precision corpus shrank to {} cases (expected >= 40)",
        fixture.cases.len()
    );
}
