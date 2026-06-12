//! The extractor-robustness CI gate.
//!
//! `fixtures/eval/extractor_corpus.yaml` is the standing regression instrument
//! for claim extraction + verdict: the same ground-truth facts phrased many
//! ways, plus every false-positive class dogfooding has caught. Until now the
//! corpus existed but NOTHING ran it. This test does, and gates on it.
//!
//! Bands and their contract (encoded in the case-name prefix):
//!   T* TRUE state claims        -> supported     (must pass 100%)
//!   F* FALSE claims (lies)      -> contradicted  (must pass 100%)
//!   D* dependency claims        -> as labelled   (must pass 100%)
//!   S* symbol/member claims     -> as labelled   (must pass 100%)
//!   P* prose collisions         -> inconclusive  (must pass 100% — a regression
//!                                  here means the verifier invented a verdict)
//!   R* vague/judgment           -> inconclusive  (must pass 100%)
//!   H* hard recall edge cases   -> supported     (now GATED — these natural
//!                                  phrasings were recall gaps until 2026-06-12;
//!                                  the whole corpus is a 100% contract so they
//!                                  can't silently regress)
//!
//! ANY case failing is a real regression — an invented verdict, a missed catch,
//! or lost recall — and fails CI.

use std::path::{Path, PathBuf};
use truth_cli::eval::{load_fixture, run_eval, Fixture};
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

/// Load the corpus and rewrite each case's relative `repo` to an absolute path,
/// so the test never depends on (or mutates) the process working directory —
/// tests run in parallel and `set_current_dir` would race.
fn load_corpus() -> Fixture {
    let root = repo_root();
    let text = std::fs::read_to_string(root.join("fixtures/eval/extractor_corpus.yaml")).unwrap();
    let mut fixture = load_fixture(&text).unwrap();
    for case in &mut fixture.cases {
        if let Some(repo) = &case.repo {
            case.repo = Some(root.join(repo).to_string_lossy().into_owned());
        }
    }
    fixture
}

#[test]
fn extractor_corpus_is_perfect() {
    let fixture = load_corpus();
    assert!(
        fixture.cases.len() >= 40,
        "corpus shrank unexpectedly: {} cases",
        fixture.cases.len()
    );

    let report = run_eval(&offline(), &fixture).unwrap();

    let failures: Vec<&_> = report.cases.iter().filter(|c| !c.passed).collect();

    if !failures.is_empty() {
        let detail: String = failures
            .iter()
            .map(|c| {
                format!(
                    "\n  {:<24} expected {:<13} got {}",
                    c.name, c.expected_status, c.actual_status
                )
            })
            .collect();
        panic!(
            "{} extractor-corpus case(s) regressed (invented verdict, missed catch, or lost recall):{}",
            failures.len(),
            detail
        );
    }
}
