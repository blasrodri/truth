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
//!   H* hard recall edge cases   -> supported     (tracked; a documented ceiling
//!                                  of bare-regex recall is tolerated)
//!
//! A failure in any GATED band (everything except H*) is a real regression — an
//! invented verdict or a missed catch — and fails CI.

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

/// `H*` cases probe phrasings the bare-regex extractor is known not to catch
/// yet; they are tracked, not gated. Everything else is a hard contract.
fn is_gated(name: &str) -> bool {
    !name.starts_with('H')
}

#[test]
fn extractor_corpus_gated_bands_are_perfect() {
    let fixture = load_corpus();
    assert!(
        fixture.cases.len() >= 40,
        "corpus shrank unexpectedly: {} cases",
        fixture.cases.len()
    );

    let report = run_eval(&offline(), &fixture).unwrap();

    let gated_failures: Vec<&_> = report
        .cases
        .iter()
        .filter(|c| is_gated(&c.name) && !c.passed)
        .collect();

    if !gated_failures.is_empty() {
        let detail: String = gated_failures
            .iter()
            .map(|c| {
                format!(
                    "\n  {:<24} expected {:<13} got {}",
                    c.name, c.expected_status, c.actual_status
                )
            })
            .collect();
        panic!(
            "{} gated extractor-corpus case(s) regressed (invented verdict or missed catch):{}",
            gated_failures.len(),
            detail
        );
    }
}

#[test]
fn extractor_corpus_hard_band_recall_does_not_collapse() {
    // The H* band is the honest recall map — allowed to have misses, but if it
    // drops below a floor something broke broadly. This guards against a change
    // that "passes the gate" by making the extractor refuse everything.
    let fixture = load_corpus();
    let report = run_eval(&offline(), &fixture).unwrap();

    let hard: Vec<&_> = report
        .cases
        .iter()
        .filter(|c| c.name.starts_with('H'))
        .collect();
    let hard_pass = hard.iter().filter(|c| c.passed).count();
    // Floor: at least one H* case must still resolve. (Today the bare regex
    // catches a subset; the point is to notice if recall goes to zero.)
    assert!(
        hard_pass >= 1,
        "hard-band recall collapsed to {hard_pass}/{} — extractor likely over-refusing",
        hard.len()
    );
}
