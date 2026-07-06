#!/usr/bin/env python3
"""Clean recall analysis on the FP-fixed replay hits.

Per-instance recall = (failed-task instances where truth caught >=1 concrete
over-claim) / (failed-task instances replayed). A "catch" here is any
contradiction, since the replay resolves each claim against the patched tree
the agent actually produced; the FP fix removed the narrative noise.
"""
import json, sys
from collections import Counter

out = sys.argv[1] if len(sys.argv) > 1 else "recall_v2.out"
hits = [json.loads(l) for l in open("replay_hits.jsonl")]

replayed = set()
for line in open(out):
    if ": +" in line:
        replayed.add(line.strip().split(":")[0].strip())

caught = set(h["instance_id"] for h in hits)
print(f"failed-task instances replayed: {len(replayed)}")
print(f"instances with >=1 contradiction (catch): {len(caught)}")
if replayed:
    print(f"per-instance recall: {100*len(caught)/len(replayed):.0f}%")
print(f"total contradictions: {len(hits)}")

# citation breakdown to show quality
def kind(c):
    cit = (c or "").lower()
    if not cit: return "no_citation"
    if any(d in cit for d in [".md",".rst","readme","contributing","changelog"]): return "doc_file"
    if "git:" in cit or "recorded" in cit: return "git/run"
    return "source_file"
kc = Counter(kind(h.get("citation")) for h in hits)
print("citation types:", dict(kc))
print("\nsample catches:")
for h in hits[:12]:
    print(f"  [{h['instance_id']}] {h['claim'][:60]} <- {(h.get('citation') or '')[-40:]}")
