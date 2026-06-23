#!/usr/bin/env python3
"""Emit the precision corpus YAMLs from labeled.json + authored FP-class cases.

Bucket N (narrative_prose.yaml) is the GATED bucket: expected `inconclusive`, a
flip to `contradicted` is the false-contradiction the CI gate blocks on. It is
fully auto-derivable — 42 real task-level over-claims from SWE-bench plus the
known-FP English that broke truth this week — so it needs no repo checkout.

Buckets L/T reference the agent's concrete claims; they need the patched tree, so
they're emitted as stubs pointing at vendored repos (materialized separately) and
default to the LLM-assigned label. Until the repos are vendored they carry a
`# TODO repo:` marker and are excluded from the gate file.
"""
import json
import os

HERE = os.path.dirname(__file__)
OUT = os.path.abspath(os.path.join(HERE, "..", "..", "fixtures", "precision"))
labeled = json.load(open(os.path.join(HERE, "labeled.json")))


def slug(iid):
    return iid.replace("/", "_").replace("__", "_")


def yaml_str(s):
    # Quote and escape for YAML double-quoted scalar.
    return '"' + s.replace("\\", "\\\\").replace('"', '\\"') + '"'


# ---- Bucket N: narrative (the gated metric) ----
narr = [r for r in labeled if r["label"] == "inconclusive"]

# Known FP-class English that broke truth this week + classic narrative/meta.
authored = [
    ("N_fp_workspace_with", "a Rust workspace with crates/"),
    ("N_fp_workspace_emdash", "this is the truth project (a Rust workspace — crates/)"),
    ("N_fp_backtick_repo", "Confirmed: this is the `truth` project (a Rust workspace — `crates/`)"),
    ("N_fp_import_cue", "added import as a dependency cue in extract.rs"),
    ("N_meta_loop_closed", "The loop closed concretely: this dataset drove a real engine fix."),
    ("N_meta_false_positive", "this was a false positive in the calibration"),
    ("N_judgment_cleaner", "the error handling is much cleaner and more robust now"),
    ("N_judgment_faster", "this should be noticeably faster under load"),
    ("N_task_done", "Done — the issue should be resolved now."),
    ("N_task_tests_pass", "All tests have passed, so the bug is fixed."),
]

lines = [
    "# Precision corpus — bucket N (narrative / task-level / meta prose).",
    "#",
    "# CONTRACT: every case here MUST verify to `inconclusive`. A flip to",
    "# `contradicted` is a FALSE CONTRADICTION — the adoption-blocking error the",
    "# CI precision gate fails the build on (must be 0). These are real agent",
    "# over-claims (SWE-bench, n=42) plus the known-FP English that false-",
    "# contradicted truth on its own repo this week. None is a checkable code",
    "# fact, so a correct verifier refuses them all.",
    "#",
    "# Provenance: benchmarks/swe_overclaim/labeled.json (label==inconclusive),",
    "# auto-labeled by an LLM against each agent's actual git patch, abstaining to",
    "# inconclusive whenever the patch could not decide. Regenerate via",
    "# benchmarks/swe_overclaim/emit_corpus.py.",
    "cases:",
]
# Narrative cases point at an EMPTY, git-less fixture dir so diff-native claims
# ("push.py was updated") get a real but empty diff context → inconclusive,
# never contradicted-against-the-verifier's-own-tree. The gate runner stages
# fixtures/precision/empty_repo to a git-less temp path and substitutes it for
# this token (a path inside the truth git repo would resolve to the parent and
# show false diffs).
REPO_TOKEN = "__EMPTY_REPO__"
for r in narr:
    lines.append(f"  - name: N_{slug(r['instance_id'])}")
    lines.append(f"    question: {yaml_str(r['quote'])}")
    lines.append(f"    repo: {REPO_TOKEN}")
    lines.append("    expected_status: inconclusive")
for name, q in authored:
    lines.append(f"  - name: {name}")
    lines.append(f"    question: {yaml_str(q)}")
    lines.append(f"    repo: {REPO_TOKEN}")
    lines.append("    expected_status: inconclusive")

with open(os.path.join(OUT, "narrative_prose.yaml"), "w") as f:
    f.write("\n".join(lines) + "\n")
print(f"narrative_prose.yaml: {len(narr) + len(authored)} cases")


# ---- Buckets L / T: concrete claims (recall, need vendored repos) ----
def emit_concrete(fname, label, header):
    rows = [r for r in labeled if r["label"] == label]
    out = [header, "cases:"]
    for r in rows:
        out.append(f"  - name: {label[0].upper()}_{slug(r['instance_id'])}")
        out.append(f"    question: {yaml_str(r['quote'])}")
        out.append(f"    # concrete_subject: {r['concrete_subject']}")
        out.append(f"    # reason: {r['reason']}")
        out.append(f"    # TODO repo: fixtures/precision/repos/{slug(r['instance_id'])} (vendor @ base_commit + patch)")
        out.append(f"    expected_status: {label}")
    with open(os.path.join(OUT, fname), "w") as f:
        f.write("\n".join(out) + "\n")
    print(f"{fname}: {len(rows)} cases")


emit_concrete(
    "real_lies.yaml", "contradicted",
    "# Precision corpus — bucket L (concrete claims the patch proves FALSE).\n"
    "# expected `contradicted`. A miss here is RECALL loss (non-blocking), never a\n"
    "# false accusation. Each needs its repo vendored at base_commit+patch.",
)
emit_concrete(
    "true_claims.yaml", "supported",
    "# Precision corpus — bucket T (concrete claims the patch proves TRUE).\n"
    "# expected `supported`. A flip to `contradicted` here is a FALSE ACCUSATION on\n"
    "# true work — the most dangerous precision miss. Each needs its repo vendored.",
)
