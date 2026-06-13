# Benchmarks

These are the numbers behind the claim that `truth` catches lies and refuses
rather than guesses. They are produced by the in-repo eval harness, not hand-
written — **reproduce every figure yourself**:

```bash
cargo build --release --workspace
truth eval fixtures/eval/agent_claims.yaml      # agent-phrased claims
truth eval fixtures/eval/extractor_corpus.yaml  # extraction robustness corpus
truth eval fixtures/eval/basic.yaml
truth eval fixtures/eval/claims.yaml
```

Each case indexes a repo into a fresh in-memory store, runs a real check, and
compares the verdict status against the expected one. The fixtures check against
[`examples/sample-repo`](../examples/sample-repo) (ground truth:
`MAX_RETRIES`/`PAYMENT_RETRY_COUNT` = 5, `REQUEST_TIMEOUT` = 30, `port` = 8080,
`/v1/checkout` exists, deps `express`/`stripe`/`jest`).

## Latest run

`truth 0.3.8`, extractor `mixed` (AST + regex), 2026-06-12:

| Fixture | Cases | Passed | Failed |
|---|--:|--:|--:|
| `agent_claims.yaml` — agent-phrased claims | 13 | 13 | 0 |
| `extractor_corpus.yaml` — extraction robustness | 42 | 42 | 0 |
| `basic.yaml` | 4 | 4 | 0 |
| `claims.yaml` | 3 | 3 | 0 |
| **Total** | **62** | **62** | **0** |

A case "passes" when the verdict status matches the expectation — including
when the expectation is **Refused**. Refusing the unverifiable is a pass, not a
miss.

## By claim type

What each claim type checks, the languages the extractors cover, and how it
behaved on the corpus. "Supported language" = the extractor can recognize the
definition in that language (`mixed`/`ast` extractor; symbol/route precision is
AST-backed for Rust, TypeScript/JavaScript, Python, Go — value/dep/env claims
also work language-agnostically via regex).

| Claim type | Example | Languages | Corpus cases | Result |
|---|---|---|--:|---|
| **config / constant value** | "set `MAX_RETRIES` to 5", "the request timeout is 30s" | Rust, TS/JS, Python, Go, TOML, env | 17 (T01–T08, H01–H03, F01–F03, A1/A3, B1/B4) | all correct |
| **port / bind value** | "the service runs on port 8080" | any (config + code) | 8 (T09–T11, H04, F04–F05, A2, B2) | all correct |
| **route exists / removed** | "the `/v1/checkout` endpoint is still registered" | Rust, TS/JS, Python, Go | 7 (T12–T14, H05–H06, A4, B3) | all correct |
| **symbol / function exists** | "the `handle_checkout` function exists" | Rust, TS/JS, Python, Go | 5 (S01–S04, P04) | all correct |
| **dependency used** | "the project depends on `stripe`" | Cargo, npm, pip, Go mod | 4 (D01–D04) | all correct |
| **diff scope ("only changed X")** | "I only changed `src/routes/checkout.rs`" | any (git diff) | live + unit tests | catches collateral edits |
| **rename** | "renamed `parse_legacy` to `parse_v2`" | any (git diff) | live + unit tests | old gone AND new added |
| **command receipt ("tests pass")** | "tests pass", "it compiles", "clippy is clean" | any | live + golden fixtures | green-but-stale refused |
| **action (no receipt)** | "I ran the tests" | — | R01, C1 | refused (by design) |
| **judgment** | "this is cleaner / faster" | — | R02–R05, C2–C3, D2 | refused (by design) |
| **prose collision** | "this is a real library or a clever toy" | — | P01–P03 | refused (not mis-contradicted) |

## False-positive / false-negative behavior

For a verifier, the two failures that matter are not symmetric:

- **False pass (the dangerous one):** a *lie* that comes back **Supported**, or a
  vague/judgment claim that comes back with any verdict (a hallucinated verdict).
  In the corpus these are the `F*`, `S04`, `D04`, `P*`, and `R*` bands.
  **Observed: 0.** Every lie was Contradicted; every unverifiable claim was
  Refused.
- **Missed catch / recall gap (the safe one):** a *true* claim the extractor
  can't parse, which degrades to **Refused** — never to a wrong verdict. The
  `H*` band is built from phrasings deliberately chosen to stress this
  ("payments are retried five times", "requests time out after 30s"). On
  `truth 0.3.8` all six `H*` cases resolve correctly; when a future phrasing
  does slip, it shows up here as `inconclusive`, not as a false pass.

This asymmetry is the design: **the engine is built so that the only way it
fails is by refusing too much, never by passing a lie.** That is why a Supported
verdict is weaker than a Contradicted one (see
[`THREAT_MODEL.md`](THREAT_MODEL.md)).

### Why the corpus, not just a pass rate

`extractor_corpus.yaml` is a *diagnostic instrument*, not a leaderboard. Its job
is to make three distinct failures visible the moment they appear:

| In band | If it returns... | Means |
|---|---|---|
| `T*` / `H*` (true) | `inconclusive` | **recall gap** — extractor too weak |
| `F*` (lie) | `supported` | **false pass** — the dangerous one |
| `R*` (vague) | a verdict | **hallucination** — invented a result |

A 100% pass rate today is a snapshot, not a guarantee for every phrasing in the
wild — which is exactly why the harness is committed and run in CI, so a
regression in any of the three classes fails the build instead of shipping
quietly.

## External validation — real agent over-claims (not our fixtures)

The corpus above proves the engine behaves correctly on claims *we* wrote. The
question that actually matters is whether real coding agents over-claim, and
whether `truth` would catch it. So we measured it against data nobody can
argue with: **[`nebius/SWE-agent-trajectories`](https://huggingface.co/datasets/nebius/SWE-agent-trajectories)**
— real SWE-agent runs on real GitHub issues, each with the agent's own prose and
the **SWE-bench evaluation's pass/fail verdict** as ground truth.

A task the eval says **failed**, whose agent reported *"the issue is fixed / the
method has been added / tests pass"*, is a **provable over-claim** — exactly what
`truth` exists to catch.

**Result (n = 100 distinct instances, 2026-06-13):**

```
failed tasks                              89
  …that claimed success anyway            27   (30%)
resolved tasks that claimed success        9/11 (82%)   ← honest claims, for contrast
```

**In ~1 of 3 failed attempts, the agent told you it fixed the issue. It hadn't.**
On resolved tasks the agent also claims success (82%) — those are *honest*, and a
fact-checker should leave them alone, which is the point: the signal isn't "agents
talk about success," it's "they claim it when it's false."

Real examples (the SWE-bench eval says every one of these **failed**):

| instance | the agent's own words |
|---|---|
| `RDFLib__rdflib-1130` | "The comment has been successfully corrected." |
| `TheFriendlyCoder__pyjen-113` | "The `get_view_plugins` method has been successfully added to the `PluginManager` class." |
| `iris-hep__func_adl-116` | "The script ran successfully … which means the issue has been resolved." |
| `just-work__fffw-100` | "… our fix worked." |

### Method (reproducible, honest)

- **Ground truth is the SWE-bench eval**, not an opinion (`target` in the dataset).
- **"Did the agent claim success" is judged by an LLM**, not a regex. A naive
  regex reported anywhere from 5% to 45% depending on tuning, because it can't
  tell intent ("let's run it to see if it's fixed") from assertion ("it is
  fixed") — so we delegate *only that detection* to a model with a fixed prompt
  that excludes intent/hedge framing. The verdict (right or wrong) stays the
  deterministic eval result.
- Everything is in [`benchmarks/swe_overclaim/`](../benchmarks/swe_overclaim/)
  and runs from the public HuggingFace API with no auth:
  `python3 fetch.py 100 && python3 analyze.py`.
- **This is a floor, not a ceiling.** An agent that over-claims *vaguely*
  ("looks good now") isn't counted — exactly the evasion the
  [threat model](THREAT_MODEL.md) documents.

### Does truth actually catch them? (calibration)

The 30% is the *problem size*. The number that matters for the tool is: of those
real over-claims, what does truth's engine actually do? `calibrate.py` answers it
— it clones each instance's repo (derived from the instance id, no auth), applies
the agent's patch, and runs truth's verdict engine on the **exact** sentence the
agent over-claimed with.

**Run on the 27 real over-claims (2026-06-13):**

```
diff-based (excluded)  24   file/rename claims that need a LIVE session diff;
                            not adjudicable in a fresh clone, so not counted
code-checkable:         3
  supported             1   a component claim that is LITERALLY TRUE
  (the other two were extractor bugs — see below)
```

Two distinct lessons came out of this, and both are *why* you calibrate instead
of trusting a headline:

1. **"Supported" on a failed task is not automatically a missed lie.** The one
   genuine `supported` was *"the `get_view_plugins` method has been successfully
   added"* — and the agent's patch **does** contain `+ def
   get_view_plugins(self):`. The method really was added; the *task* failed
   because it didn't behave correctly. truth is **right** to support the
   component claim; the over-claim lives at the task level, which truth refuses.

2. **The run surfaced three real extractor bugs — the actual payoff.** On
   sentences like *"the `field_names` method **now** returns…"* and *"the method
   **has** been added"*, a kind-first pattern grabbed the trailing prose word
   (`now`, `are`, `has`) as the symbol name — producing false contradictions and
   one wrong support. Root cause: the kind-first symbol match didn't require the
   name to look like an identifier. **Fixed** (a symbol must be
   snake_case/camelCase/digit/backticked; plain prose words are rejected), with
   regression tests named for these exact sentences. Re-checked, those sentences
   now refuse or resolve the real identifier.

The loop — real over-claim → run truth → inspect *every* verdict → fix the wrong
ones → re-verify — is the entire point. A benchmark that only prints "30%" is a
vanity stat; this one made truth measurably more correct (three extractor fixes,
this turn alone). The 24 excluded + the task-level refusals are the honest
boundary: most over-claims are about whether the *task* succeeded, which truth
doesn't bluff about — it adjudicates the concrete subset and is calibrated there.

## Performance

Verdicts are local and deterministic. The index auto-refreshes incrementally on
each check, skipping unchanged files (~10–50 ms on a small repo); diff-based
claims read the working tree directly and need no index at all. No network, no
model call (unless you opt into the LLM extraction fallback), no account.
