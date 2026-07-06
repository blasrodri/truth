//! Field false-positive corpus — every `contradicted` verdict truth issued in
//! real agent sessions (June–July 2026 session-log audit) was a FALSE
//! accusation: 13/13. Each cost the agent several turns refuting truth and
//! ended with "my own grep is authoritative over truth" — adoption death.
//!
//! Each test below reconstructs one field FP as a minimal temp git repo
//! (committed state + working-tree edits) and drives the exact path the field
//! calls used: `verify_claims` with an agent-supplied `claims` array, as the
//! MCP `verify_turn` tool does. The contract for every case: the claim must
//! NOT verify to `contradicted`. Supported is ideal, refused/inconclusive is
//! acceptable — a false accusation is the only failure.
//!
//! FP classes covered (from the audit):
//!   1. doc-keyword citations — CHANGELOG.md / README.md / AGENTS.md /
//!      benches/* text latched as contradiction evidence for a CODE claim
//!   2. same-hunk confusion — "I added Org/Role/OrgMembership" contradicted
//!      because `Group` was REMOVED in the same hunk
//!   3. tests-added-in-diff — "I added tests for X to <file>" contradicted
//!      while the working diff plainly adds them
//!   4. negative epistemic claims — "I have NOT verified X" contradicted by
//!      unrelated doc text

use std::path::{Path, PathBuf};
use std::process::Command;
use truth_cli::verify_turn::{retarget_repo, verify_claims};
use truth_core::config::Config;
use truth_core::enums::VerdictStatus;

/// A scratch git repo: `committed` files are committed at HEAD, then `edits`
/// are applied to the working tree (uncommitted), mirroring an agent mid-turn.
struct FieldRepo {
    root: PathBuf,
}

impl FieldRepo {
    fn new(name: &str, committed: &[(&str, &str)], edits: &[(&str, &str)]) -> Self {
        let root =
            std::env::temp_dir().join(format!("truth_field_fp_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&["init", "-q"]);
        for (path, content) in committed {
            let p = root.join(path);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, content).unwrap();
        }
        git(&["add", "-A"]);
        git(&["commit", "-qm", "base", "--no-gpg-sign"]);
        for (path, content) in edits {
            let p = root.join(path);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, content).unwrap();
        }
        Self { root }
    }

    /// Verify agent-supplied claims exactly as the MCP tool does; return the
    /// (claim text, status) pairs.
    fn verify(&self, claims: &[&str]) -> Vec<(String, VerdictStatus)> {
        let mut config = Config::from_toml_str("").unwrap();
        config.loki.enabled = false;
        retarget_repo(&mut config, &self.root.to_string_lossy());
        let conn = truth_db::open(&config.database.path).unwrap();
        let list: Vec<String> = claims.iter().map(|c| c.to_string()).collect();
        let report = verify_claims(&conn, &config, &list.join(". "), Some(&list), None).unwrap();
        report
            .verdicts
            .into_iter()
            .map(|v| (v.text, v.status))
            .collect()
    }
}

impl Drop for FieldRepo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Assert the field contract: none of these claims may contradict.
fn assert_no_contradiction(case: &str, verdicts: &[(String, VerdictStatus)]) {
    let offenders: Vec<&(String, VerdictStatus)> = verdicts
        .iter()
        .filter(|(_, s)| *s == VerdictStatus::Contradicted)
        .collect();
    assert!(
        offenders.is_empty(),
        "{case}: FALSE ACCUSATION reproduced — truth contradicted truthful claim(s): {offenders:?}"
    );
}

// --- class 1: doc-keyword citations must never contradict code claims -------

#[test]
fn changelog_keywords_do_not_contradict_code_behavior_claim() {
    // Field FP (nango 2026-06-09): "cache.ts calls multi.hIncrBy(...)" was
    // contradicted citing CHANGELOG.md:102 — an unrelated changelog entry
    // sharing the words "usage"/"cache". The actual source matched the claim
    // verbatim.
    let repo = FieldRepo::new(
        "changelog",
        &[
            (
                "CHANGELOG.md",
                "# Changelog\n- Replace BATCHER_DROPPED with ingest result for usage cache entries\n",
            ),
            (
                "packages/usage/lib/cache.ts",
                "export function incr(multi: any, key: string, delta: number) {\n  multi.hIncrBy(key, 'count', delta);\n}\n",
            ),
        ],
        &[],
    );
    let verdicts = repo.verify(&[
        "cache.ts in the usage package calls multi.hIncrBy(key, 'count', delta) to increment the counter",
    ]);
    assert_no_contradiction("changelog-keywords", &verdicts);
}

#[test]
fn readme_text_does_not_contradict_architecture_claim() {
    // Field FP (healthtrust360 2026-06-25): "There is a store package with a
    // Postgres implementation using pgx" contradicted citing README.md:12.
    // Everything was committed (clean tree) — the code plainly proved the claim.
    let repo = FieldRepo::new(
        "readme",
        &[
            (
                "README.md",
                "# ht360-gk\n\nGatekeeper.\n\n- auth\n- authz\n- users\n- groups\n- sessions\n- tokens\n- migrations\n- store: Postgres\n",
            ),
            (
                "store/postgres.go",
                "package store\n\nimport \"github.com/jackc/pgx/v5\"\n\ntype Postgres struct{ pool *pgx.Conn }\n",
            ),
        ],
        &[],
    );
    let verdicts =
        repo.verify(&["There is a store package with a Postgres implementation using pgx"]);
    assert_no_contradiction("readme-architecture", &verdicts);
}

#[test]
fn bench_file_does_not_contradict_source_claim() {
    // Field FP (tokio 2026-06-09): "idle.rs notify_should_wakeup uses
    // fetch_add(0, SeqCst) as a load" contradicted citing
    // benches/sync_broadcast.rs:59 — a different file entirely. idle.rs
    // contained the exact code claimed.
    let repo = FieldRepo::new(
        "bench",
        &[
            (
                "src/idle.rs",
                "impl Idle {\n    fn notify_should_wakeup(&self) -> bool {\n        self.state.fetch_add(0, SeqCst) != 0\n    }\n}\n",
            ),
            (
                "benches/sync_broadcast.rs",
                "fn bench(b: &mut Bencher) {\n    let counter = AtomicUsize::new(0);\n    counter.fetch_add(1, Relaxed);\n}\n",
            ),
        ],
        &[],
    );
    let verdicts = repo.verify(&[
        "idle.rs notify_should_wakeup uses fetch_add(0, SeqCst) as a load on the notify path",
    ]);
    assert_no_contradiction("bench-citation", &verdicts);
}

// --- class 2: same-hunk add/remove confusion --------------------------------

#[test]
fn added_types_not_contradicted_by_removed_type_in_same_hunk() {
    // Field FP (ht360-gk 2026-07-01): "I added Org, Role, and OrgMembership
    // types to models.go" contradicted — the same hunk REMOVED the old Group
    // type, and the engine keyed on the removal. All three types were present.
    let repo = FieldRepo::new(
        "samehunk",
        &[(
            "models.go",
            "package gk\n\ntype User struct{ ID string }\n\ntype Group struct{ ID string }\n",
        )],
        &[(
            "models.go",
            "package gk\n\ntype User struct{ ID string }\n\ntype Org struct{ ID string }\n\ntype Role struct{ Name string }\n\ntype OrgMembership struct{ OrgID string }\n",
        )],
    );
    let verdicts = repo.verify(&["I added Org, Role, and OrgMembership types to models.go"]);
    assert_no_contradiction("same-hunk", &verdicts);
}

// --- class 3: tests plainly added in the working diff -----------------------

#[test]
fn tests_added_in_working_diff_not_contradicted() {
    // Field FPs (vllm 2026-06-25, eywa 2026-06-17): "I added tests for X to
    // <file>" / "New tests were added to <file>" contradicted while git diff
    // showed the added test functions.
    let repo = FieldRepo::new(
        "testsadded",
        &[(
            "tests/v1/core/test_kv_cache_utils.py",
            "def test_existing():\n    assert True\n",
        )],
        &[(
            "tests/v1/core/test_kv_cache_utils.py",
            "def test_existing():\n    assert True\n\n\ndef test_approximate_gcd_minimizes_padding():\n    assert _approximate_gcd(8, 12) == 4\n\n\ndef test_approximate_gcd_respects_lower_bound():\n    assert _approximate_gcd(7, 13) == 1\n",
        )],
    );
    let verdicts = repo.verify(&[
        "I added tests for _approximate_gcd to tests/v1/core/test_kv_cache_utils.py",
        "New tests were added to tests/v1/core/test_kv_cache_utils.py",
    ]);
    assert_no_contradiction("tests-added", &verdicts);
}

#[test]
fn state_variable_added_in_diff_not_contradicted() {
    // Field FP (eywa 2026-06-17): "I added a copiedId state variable in
    // dashboard/pages/patients/index.js" contradicted; the edit was on disk.
    let repo = FieldRepo::new(
        "statevar",
        &[(
            "dashboard/pages/patients/index.js",
            "export default function Patients() {\n  return null;\n}\n",
        )],
        &[(
            "dashboard/pages/patients/index.js",
            "export default function Patients() {\n  const [copiedId, setCopiedId] = useState(null);\n  return null;\n}\n",
        )],
    );
    let verdicts =
        repo.verify(&["I added a copiedId state variable in dashboard/pages/patients/index.js"]);
    assert_no_contradiction("state-variable", &verdicts);
}

#[test]
fn view_kwargs_present_in_diff_not_contradicted() {
    // Field FP (lowex-backend 2026-06-22): "The enrich_cuenta view now passes
    // values=changed and updated_fields=list(changed.keys())" contradicted with
    // no citation at all; the lines were at views.py:289-290.
    let repo = FieldRepo::new(
        "kwargs",
        &[(
            "leads/views.py",
            "def enrich_cuenta(request, pk):\n    return EnrichResult()\n",
        )],
        &[(
            "leads/views.py",
            "def enrich_cuenta(request, pk):\n    changed = compute_changed(request)\n    return EnrichResult(values=changed, updated_fields=list(changed.keys()))\n",
        )],
    );
    let verdicts = repo.verify(&[
        "The enrich_cuenta view now passes values=changed and updated_fields=list(changed.keys())",
    ]);
    assert_no_contradiction("view-kwargs", &verdicts);
}

// --- class 4: negative epistemic claims (admissions) -------------------------

#[test]
fn admission_of_not_verifying_is_refused_never_contradicted() {
    // Field FP (truth/vllm 2026-06-30): "I have not verified the embedded
    // interpreter can import the vllm package" contradicted citing AGENTS.md:3
    // ("These instructions apply to all AI-assisted contributions") — surface
    // token overlap with a policy doc. An admission has nothing to catch.
    let repo = FieldRepo::new(
        "admission",
        &[(
            "AGENTS.md",
            "# Contributing\n\nThese instructions apply to all AI-assisted contributions to vllm.\n",
        )],
        &[],
    );
    let verdicts = repo.verify(&[
        "I have not verified the embedded interpreter can import the vllm package",
        "I did not run the test suite",
    ]);
    assert_no_contradiction("admission", &verdicts);
    // Stronger contract for admissions: they are refused (not checkable), so
    // they can never be judged at all.
    for (text, status) in &verdicts {
        assert_ne!(
            *status,
            VerdictStatus::Supported,
            "admission must not be judged either way: {text}"
        );
    }
}

// --- committed-work fallback (field audit: mass refusals on clean trees) ----

#[test]
fn committed_work_resolves_against_head_instead_of_blanket_refusal() {
    // Field case (healthtrust360 2026-06-25): everything was committed before
    // verify_turn ran → clean tree → every file claim refused. The HEAD commit
    // is evidence: a file changed by it supports "I created/edited X"; a file
    // that doesn't exist at all contradicts it.
    let repo = FieldRepo::new(
        "committed",
        &[
            ("store/postgres.go", "package store\n"),
            ("gatekeeper.go", "package gk\n"),
        ],
        &[],
    );
    let verdicts = repo.verify(&[
        "I created store/postgres.go",
        "I edited gatekeeper.go",
        "I created httpmw/middleware.go", // never existed → sound contradiction
    ]);
    assert_eq!(
        verdicts[0].1,
        VerdictStatus::Supported,
        "committed creation must resolve via HEAD: {verdicts:?}"
    );
    assert_eq!(
        verdicts[1].1,
        VerdictStatus::Supported,
        "committed edit must resolve via HEAD: {verdicts:?}"
    );
    assert_eq!(
        verdicts[2].1,
        VerdictStatus::Contradicted,
        "a file that never existed is a sound structured contradiction: {verdicts:?}"
    );
}

#[test]
fn file_outside_last_commit_stays_inconclusive_on_clean_tree() {
    // Two commits: base.rs in the first, other.rs in HEAD. A clean-tree claim
    // about base.rs can't be attributed to "this turn" → inconclusive, never
    // supported (that would false-pass) and never contradicted.
    let repo = FieldRepo::new("twocommits", &[("base.rs", "fn a() {}\n")], &[]);
    let git = |args: &[&str]| {
        assert!(Command::new("git")
            .arg("-C")
            .arg(&repo.root)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap()
            .status
            .success());
    };
    std::fs::write(repo.root.join("other.rs"), "fn b() {}\n").unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-qm", "second", "--no-gpg-sign"]);

    let verdicts = repo.verify(&["I edited base.rs"]);
    assert_eq!(
        verdicts[0].1,
        VerdictStatus::Inconclusive,
        "can't attribute an old committed file to this turn: {verdicts:?}"
    );
}

/// Guard: the corpus keeps covering all four field FP classes. (A deleted test
/// would silently shrink coverage; this makes the file's intent explicit.)
#[test]
fn corpus_covers_all_field_fp_classes() {
    let me = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/field_fp_corpus.rs"),
    )
    .unwrap();
    for class in ["class 1", "class 2", "class 3", "class 4"] {
        assert!(me.contains(class), "field FP {class} coverage removed");
    }
}
