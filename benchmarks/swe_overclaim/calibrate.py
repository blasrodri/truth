#!/usr/bin/env python3
"""Calibrate truth against real agent over-claims — the POINT of the benchmark.

The 30% number proves agents over-claim. This turns that into a calibration
loop: run truth's actual engine over the agent's CONCRETE claims, against the
repo as the agent left it, and bucket every verdict:

  CAUGHT      truth Contradicted a claim on a FAILED task  → a real catch ✓
  REFUSED     truth couldn't check it (task-level "it's fixed") → honest, not a miss
  FALSE PASS  truth Supported a claim on a FAILED task      → the DANGEROUS miss ✗✗
  (on resolved tasks, Supported is correct and Contradicted is a false accusation)

Every FALSE PASS and every false accusation is a calibration signal — a concrete
sentence + repo state where truth got it wrong, to fix (exactly like the has/needs
extractor bugs truth caught on its own summary).

Repo is derived from the instance_id (`owner__repo-NNN` → github.com/owner/repo),
so we DON'T need the rate-limited HF lookup for it. base_commit is fetched once
with backoff (optional — we fall back to the default branch + patch).

Usage: python3 calibrate.py [trajectories.jsonl] [max=20] [truth_bin]
"""
import json
import os
import subprocess
import sys
import tempfile
import time
import urllib.parse
import urllib.request

TRUTH = os.path.expanduser("~/.cargo/bin/truth")
BASE = "https://datasets-server.huggingface.co"


def run(cmd, cwd=None, timeout=300):
    return subprocess.run(cmd, cwd=cwd, timeout=timeout, capture_output=True, text=True)


def repo_of(instance_id):
    """`owner__repo-NNN` -> `owner/repo` (GitHub slug)."""
    base = instance_id.rsplit("-", 1)[0]
    return base.replace("__", "/", 1)


def base_commit(instance_id):
    """Fetch base_commit from SWE-bench-extra, best-effort with backoff."""
    where = urllib.parse.quote(f"\"instance_id\"='{instance_id}'")
    url = (
        f"{BASE}/filter?dataset=nebius%2FSWE-bench-extra"
        f"&config=default&split=train&where={where}&length=1"
    )
    for i in range(4):
        try:
            with urllib.request.urlopen(url, timeout=30) as r:
                rows = json.load(r).get("rows", [])
                if rows:
                    return rows[0]["row"].get("base_commit")
                return None
        except Exception:  # noqa: BLE001
            time.sleep(5 * (i + 1))
    return None


# base_commit fidelity is opt-in: looking it up hits the rate-limited HF API.
# By default we clone the repo's current default branch and apply the agent
# patch — good enough to adjudicate symbol/value existence claims, and with NO
# external rate-limit dependency. Set USE_BASE_COMMIT=1 for exact fidelity.
USE_BASE_COMMIT = os.environ.get("USE_BASE_COMMIT") == "1"


def setup_repo(instance_id, patch_text, dest):
    """Clone owner/repo (default branch, or base_commit if opted in), apply patch."""
    slug = repo_of(instance_id)
    url = f"https://github.com/{slug}.git"
    run(["git", "init", "-q", dest])
    run(["git", "-C", dest, "remote", "add", "origin", url])
    commit = base_commit(instance_id) if USE_BASE_COMMIT else None
    fetched = False
    if commit:
        if run(["git", "-C", dest, "fetch", "-q", "--depth", "1", "origin", commit]).returncode == 0:
            run(["git", "-C", dest, "checkout", "-q", commit])
            fetched = True
    if not fetched:
        # Default branch (shallow) — no HF lookup, no rate limit.
        if run(["git", "-C", dest, "fetch", "-q", "--depth", "1", "origin", "HEAD"]).returncode != 0:
            return False
        run(["git", "-C", dest, "checkout", "-q", "FETCH_HEAD"])
    # Apply the agent's patch (the code state it is claiming about). Best-effort.
    if patch_text.strip():
        pf = os.path.join(dest, ".agent.patch")
        open(pf, "w").write(patch_text)
        for args in (["apply"], ["apply", "--3way"], ["apply", "--reject", "--whitespace=fix"]):
            if run(["git", "-C", dest, *args, ".agent.patch"]).returncode == 0:
                break
    return True


# The claims to check are the EXACT over-claim sentences the LLM judge flagged
# (judge.py / judged_cache.json) — not a regex over all prose. That's the whole
# point: of the sentences where the agent claimed success on a FAILED task, how
# many does truth's engine adjudicate (catch / refuse), and does it ever
# false-pass one? Feeding truth mid-task exploration ("let's fix...") just yields
# refusals and measures nothing.
def judged_overclaim(instance_id):
    """The success-claim sentence the LLM judge extracted for this instance."""
    try:
        cache = json.load(open(os.path.join(os.path.dirname(__file__), "judged_cache.json")))
    except Exception:  # noqa: BLE001
        return None
    v = cache.get(instance_id)
    if v and v.get("claimed_success") and v.get("quote"):
        return v["quote"]
    return None


# Claim types whose verdict is TRUSTWORTHY in a fresh clone: they check against
# the INDEXED CODE (does this symbol/value/route exist?). Diff-based types
# (file_changed / only_changed / change_count / rename) check `git diff HEAD`,
# which in a clone+patch does NOT reflect "what the agent changed in its session"
# — so their verdicts here are setup artifacts, not real signal. We record them
# but don't count them toward catch/false-pass.
CODE_TYPES = {
    "symbol_exists",
    "config_value",
    "retry_count",
    "timeout_value",
    "version_required",
    "route_exists",
    "env_var_exists",
    "dependency_used",
}


def truth_verdicts(dest, claims):
    run([TRUTH, "init"], cwd=dest)
    run([TRUTH, "index"], cwd=dest)
    buckets = {"supported": 0, "contradicted": 0, "refused": 0, "diff_skipped": 0}
    details = []
    for c in claims:
        r = run([TRUTH, "check", c, "--json"], cwd=dest, timeout=60)
        try:
            v = json.loads(r.stdout)
            st = v["status"]
            ctype = (v.get("claim") or {}).get("claim_type", "unknown")
        except Exception:  # noqa: BLE001
            st, ctype = "refused", "unknown"
        # Only code-checkable claim types yield trustworthy verdicts in a clone.
        if ctype not in CODE_TYPES and st != "refused":
            buckets["diff_skipped"] += 1
            details.append({"claim": c, "status": "diff_skipped", "claim_type": ctype})
            continue
        if st == "supported":
            buckets["supported"] += 1
        elif st == "contradicted":
            buckets["contradicted"] += 1
        else:
            buckets["refused"] += 1
        details.append({"claim": c, "status": st, "claim_type": ctype})
    return buckets, details


def main():
    path = sys.argv[1] if len(sys.argv) > 1 else "trajectories.jsonl"
    cap = int(sys.argv[2]) if len(sys.argv) > 2 else 20
    global TRUTH
    if len(sys.argv) > 3:
        TRUTH = sys.argv[3]

    rows = [json.loads(line) for line in open(path)]
    # Calibrate on FAILED tasks that the judge says claimed success — those are
    # the real over-claims. Check the EXACT sentence the agent over-claimed with.
    targets = []
    for r in rows:
        if r["resolved"]:
            continue
        quote = judged_overclaim(r["instance_id"])
        if quote:
            targets.append((r, quote))
    targets = targets[:cap]
    print(f"calibrating truth on {len(targets)} real over-claims (failed tasks)", file=sys.stderr)

    agg = {"supported": 0, "contradicted": 0, "refused": 0, "diff_skipped": 0,
           "instances": 0, "setup_fail": 0}
    false_passes, catches = [], []
    for r, quote in targets:
        with tempfile.TemporaryDirectory(prefix="truth-cal-") as dest:
            if not setup_repo(r["instance_id"], r["generated_patch"], dest):
                agg["setup_fail"] += 1
                continue
            buckets, details = truth_verdicts(dest, [quote])
            agg["instances"] += 1
            for k in ("supported", "contradicted", "refused", "diff_skipped"):
                agg[k] += buckets[k]
            for d in details:
                if d["status"] == "supported":
                    false_passes.append({"instance_id": r["instance_id"], **d})
                elif d["status"] == "contradicted":
                    catches.append({"instance_id": r["instance_id"], **d})
            st = details[0]["status"] if details else "?"
            print(f"  {r['instance_id']}: {st}  «{quote[:60]}»", file=sys.stderr)

    total = agg["supported"] + agg["contradicted"] + agg["refused"]
    print("=" * 60)
    print("truth calibration on real agent over-claims (FAILED tasks)")
    print("=" * 60)
    print(f"instances run            {agg['instances']}  (setup failures: {agg['setup_fail']})")
    print(f"over-claim sentences     {total + agg['diff_skipped']}")
    print(f"  diff-based (excluded)  {agg['diff_skipped']}  ← file/rename claims need a LIVE session diff, not a clone")
    print(f"  -- code-checkable: --")
    print(f"  CAUGHT (contradicted)  {agg['contradicted']}  ← truth flagged a false component claim")
    print(f"  refused (can't check)  {agg['refused']}  ← task-level (\"it's resolved\"); by design")
    print(f"  supported              {agg['supported']}  ← a component claim that is LITERALLY TRUE")
    print()
    print("NOTE: 'supported' on a failed task is NOT automatically a false pass.")
    print("An agent's patch can genuinely add `def foo` (so \"foo was added\" is TRUE)")
    print("while the TASK still fails — that's a task-level over-claim truth refuses,")
    print("not a symbol lie. A real false pass is Supported where the code does NOT")
    print("contain the claimed thing; each 'supported' below is hand-verified.")
    print()
    print("--- 'supported' verdicts to hand-verify (true component claim, or real false pass?) ---")
    for f in false_passes[:12]:
        print(f"  [{f['instance_id']}] {f['claim'][:90]}")
    json.dump({"agg": agg, "supported_to_audit": false_passes, "catches": catches},
              open("calibration.json", "w"), indent=2)
    print("\nwrote calibration.json")


if __name__ == "__main__":
    main()
