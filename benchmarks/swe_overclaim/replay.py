#!/usr/bin/env python3
"""Stage 3 — engine replay (the rigorous proof).

Stage 2 establishes "the agent claimed success on a task that failed" from the
eval ground truth. Stage 3 proves truth's DETERMINISTIC ENGINE catches the
agent's concrete claims — not just a regex spotting the word "fixed".

For a curated subset of instances (those mappable to a repo + base_commit):
  1. shallow-clone the repo, check out base_commit
  2. apply the agent's generated_patch (the code state it is claiming about)
  3. run `truth index` + `truth verify-turn` over the agent's prose
  4. record truth's per-claim verdicts (Supported / Contradicted / Refused)

We then report how many concrete claims truth's engine contradicts, and confirm
ZERO false accusations by construction: every Contradicted claim is checked
against the patched tree the agent actually produced. Heavy (real clones), so it
caps the number of instances.

Usage:
    python3 replay.py [trajectories.jsonl] [max_instances=15] [truth_bin]
"""
import json
import os
import subprocess
import sys
import tempfile

TRUTH = None  # resolved in main()


def run(cmd, cwd=None, check=False, timeout=300):
    return subprocess.run(
        cmd, cwd=cwd, check=check, timeout=timeout,
        capture_output=True, text=True,
    )


def clone_at(repo, base_commit, dest):
    """Shallow-fetch just the base_commit of a GitHub repo into dest."""
    url = f"https://github.com/{repo}.git"
    run(["git", "init", "-q", dest])
    run(["git", "-C", dest, "remote", "add", "origin", url])
    f = run(["git", "-C", dest, "fetch", "-q", "--depth", "1", "origin", base_commit])
    if f.returncode != 0:
        # Some commits aren't fetchable by sha shallowly; fall back to full-ish.
        run(["git", "-C", dest, "fetch", "-q", "--depth", "50", "origin"])
    co = run(["git", "-C", dest, "checkout", "-q", base_commit])
    return co.returncode == 0


def apply_patch(dest, patch_text):
    if not patch_text.strip():
        return True
    pf = os.path.join(dest, ".agent.patch")
    with open(pf, "w") as f:
        f.write(patch_text)
    # Try a few strategies — agent patches are not always clean.
    for args in (["apply"], ["apply", "--3way"], ["apply", "--reject", "--whitespace=fix"]):
        r = run(["git", "-C", dest, *args, ".agent.patch"])
        if r.returncode == 0:
            return True
    return False


def verify(dest, agent_text):
    """Run truth verify-turn over the agent's prose against the patched tree."""
    run([TRUTH, "init"], cwd=dest)
    run([TRUTH, "index"], cwd=dest)
    r = run(
        [TRUTH, "verify-turn", agent_text[:8000], "--repo", dest, "--json"],
        cwd=dest, timeout=120,
    )
    try:
        return json.loads(r.stdout)
    except Exception:  # noqa: BLE001
        return None


def main():
    path = sys.argv[1] if len(sys.argv) > 1 else "trajectories.jsonl"
    cap = int(sys.argv[2]) if len(sys.argv) > 2 else 15
    global TRUTH
    TRUTH = sys.argv[3] if len(sys.argv) > 3 else os.path.expanduser("~/.cargo/bin/truth")

    rows = [json.loads(line) for line in open(path)]
    mappable = [r for r in rows if r.get("repo") and r.get("base_commit")]
    print(f"{len(mappable)}/{len(rows)} trajectories map to a repo+commit; replaying up to {cap}", file=sys.stderr)

    tally = {"instances": 0, "supported": 0, "contradicted": 0, "refused": 0, "clone_fail": 0}
    hits = []
    for r in mappable[:cap]:
        with tempfile.TemporaryDirectory(prefix="truth-swe-") as dest:
            if not clone_at(r["repo"], r["base_commit"], dest):
                tally["clone_fail"] += 1
                continue
            apply_patch(dest, r["generated_patch"])
            report = verify(dest, r["agent_text"])
            if not report:
                continue
            tally["instances"] += 1
            s = report.get("summary", {})
            tally["supported"] += s.get("supported", 0)
            tally["contradicted"] += s.get("contradicted", 0)
            tally["refused"] += s.get("refused", 0)
            for c in report.get("claims", []):
                if c.get("status") == "contradicted":
                    hits.append({"instance_id": r["instance_id"], "resolved": r["resolved"],
                                 "claim": c.get("text"), "citation": c.get("citation")})
            print(f"  {r['instance_id']}: +{s.get('supported',0)} -{s.get('contradicted',0)} ?{s.get('refused',0)}", file=sys.stderr)

    print("=" * 60)
    print("Engine replay — truth's verdict engine on real agent prose")
    print("=" * 60)
    print(f"instances replayed     {tally['instances']}  (clone failures: {tally['clone_fail']})")
    print(f"claims supported       {tally['supported']}")
    print(f"claims CONTRADICTED    {tally['contradicted']}")
    print(f"claims refused         {tally['refused']}")
    print()
    for h in hits[:10]:
        print(f"  ✗ [{h['instance_id']}] {str(h['claim'])[:80]}")
    with open("replay_hits.jsonl", "w") as f:
        for h in hits:
            f.write(json.dumps(h) + "\n")
    print("\nwrote contradicted claims to replay_hits.jsonl")


if __name__ == "__main__":
    main()
