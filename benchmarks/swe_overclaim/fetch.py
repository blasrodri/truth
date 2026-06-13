#!/usr/bin/env python3
"""Stage 1 — fetch SWE-agent trajectories + their SWE-bench ground truth.

The honest external benchmark: real agent attempts on real GitHub issues, where
we KNOW (from the SWE-bench eval) whether the task actually passed. A trajectory
with `target=False` whose prose claims success is a provable over-claim.

No auth required — everything comes from the public HuggingFace datasets-server
API. Writes one JSONL row per trajectory to trajectories.jsonl:

    {instance_id, resolved, repo, base_commit, generated_patch, agent_text}

`agent_text` is the concatenated first-person prose from the agent's turns —
the raw material truth's extractor + engine will judge in stage 2.
"""
import json
import sys
import time
import urllib.parse
import urllib.request

TRAJ_DS = "nebius/SWE-agent-trajectories"
SWEBENCH_DS = "princeton-nlp/SWE-bench"
BASE = "https://datasets-server.huggingface.co"


def get(url, tries=4):
    """GET JSON with naive backoff — the datasets-server rate-limits."""
    for i in range(tries):
        try:
            with urllib.request.urlopen(url, timeout=30) as r:
                return json.load(r)
        except Exception as e:  # noqa: BLE001 — best-effort fetch
            if i == tries - 1:
                raise
            time.sleep(1.5 * (i + 1))
    return None


def rows(dataset, offset, length, split="train"):
    url = (
        f"{BASE}/rows?dataset={urllib.parse.quote(dataset, safe='')}"
        f"&config=default&split={split}&offset={offset}&length={length}"
    )
    return get(url).get("rows", [])


# instance_id -> (repo, base_commit). Instances live in either the main
# SWE-bench (test split) or SWE-bench-extra (train split).
SWEBENCH_SOURCES = [
    (SWEBENCH_DS, "test"),
    ("nebius/SWE-bench-extra", "train"),
]


def swebench_lookup(instance_id, cache):
    """Map instance_id -> (repo, base_commit), trying both SWE-bench datasets."""
    if instance_id in cache:
        return cache[instance_id]
    val = (None, None)
    for ds, split in SWEBENCH_SOURCES:
        where = urllib.parse.quote(f"\"instance_id\"='{instance_id}'")
        url = (
            f"{BASE}/filter?dataset={urllib.parse.quote(ds, safe='')}"
            f"&config=default&split={split}&where={where}&length=1"
        )
        try:
            r = get(url, tries=2).get("rows", [])
        except Exception:  # noqa: BLE001
            r = []
        if r:
            val = (r[0]["row"].get("repo"), r[0]["row"].get("base_commit"))
            break
    cache[instance_id] = val
    return val


def agent_prose(trajectory):
    """First-person agent text across the trajectory (role == 'ai')."""
    return "\n".join(
        m.get("text", "")
        for m in trajectory
        if isinstance(m, dict) and m.get("role") == "ai" and m.get("text")
    )


TOTAL = 80036  # dataset size (single train split)


def main():
    n = int(sys.argv[1]) if len(sys.argv) > 1 else 200
    out_path = sys.argv[2] if len(sys.argv) > 2 else "trajectories.jsonl"
    # Sample distinct instances cheaply: stride across the dataset in PAGES
    # (100 rows/call is the server max), keeping the first trajectory per
    # instance. Repo+base_commit are looked up lazily in stage 3 (most rows are
    # SWE-bench-extra instances not in the main set, and the headline number
    # needs only `resolved`, which is in the trajectory row itself).
    # The dataset has only ~100 DISTINCT instances (each retried many times for
    # fine-tuning). To capture as many distinct ones as possible, sweep the whole
    # dataset in pages and keep the FIRST trajectory per instance. `n` caps the
    # number of distinct instances collected.
    page = 100
    written = 0
    seen = set()
    with open(out_path, "w") as out:
        offset = 0
        while written < n and offset < TOTAL:
            batch = rows(TRAJ_DS, offset, page)
            offset += page
            if not batch:
                break
            for r in batch:
                row = r["row"]
                iid = row["instance_id"]
                if iid in seen:
                    continue
                seen.add(iid)
                out.write(json.dumps({
                    "instance_id": iid,
                    "resolved": bool(row.get("target")),
                    "repo": None,
                    "base_commit": None,
                    "generated_patch": row.get("generated_patch") or "",
                    "agent_text": agent_prose(row.get("trajectory") or []),
                }) + "\n")
                written += 1
                if written >= n:
                    break
            if offset % 2000 == 0:
                print(f"  swept to offset {offset}: {written} distinct instances", file=sys.stderr)
    print(f"wrote {written} distinct instances to {out_path}", file=sys.stderr)


if __name__ == "__main__":
    main()
