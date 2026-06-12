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

## Performance

Verdicts are local and deterministic. The index auto-refreshes incrementally on
each check, skipping unchanged files (~10–50 ms on a small repo); diff-based
claims read the working tree directly and need no index at all. No network, no
model call (unless you opt into the LLM extraction fallback), no account.
