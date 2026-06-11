# Extraction Audit — how naive are we, really?

> **Status update (2026-06):** several recommendations below have since
> shipped. The **tree-sitter path (option 4 / phase 3) is built** for Rust,
> TypeScript/JS/TSX, Python, and Go (`indexer.extractor = ast|mixed`): routes
> by framework call/decorator node, typed constants, real symbol definitions —
> the 95%-false-positive route problem is gone in AST mode. A **constant
> relevance gate** (config-keyword include, hardware-marker veto) cut kernel
> noise 2.36M → 161k. The claim-extraction side also widened: file-change /
> only-changed / rename / change-count / command-success claims, and from→to
> value phrasings. Still open below: feature flags, string/bool config,
> version requirements, job schedules, `.env` values, test assertions,
> comments/docstrings, and a real usage graph (the `refs` scan is grep-level).

Status (at time of writing): **very naive.** Evidence-backed below, measured
against real repos on disk (aptos-core: 6,089 files; aria: Go; kernel-style
C). The indexer is now fast and thorough at *finding files*; the **extractor
is the product's limiting factor** and is currently the weakest link by a
wide margin.

## TL;DR

`extract.rs` is ~8 regexes matching 5 fact types (routes, numeric constants,
ports, env vars, dependencies) line-by-line, language-agnostically. It has both
a **precision** problem (massive false positives) and a **recall** problem
(misses most real claims), and it is architecturally incapable of the product's
core promise ("is X still used?").

## Evidence: precision is broken

`truth index ~/projects/aptos-core` then `truth inspect`:

| Category | Count truth reports | Reality |
|---|---:|---|
| routes | **1,240** (639 distinct) | aptos-core is a blockchain with ~a few dozen REST routes. ~95% are noise. |
| constants | **2,293** | includes `A = 1`, `A = 10` (single-letter vars), enum discriminants, abort codes |

Sample "routes" truth found:
- `/0`, `/637`, `/memory/0` — these are **crypto derivation paths**
  (`m/44'/637'/0'/0'/0'`) and array indices, not HTTP routes.
- `/transactions`, `/view`, `/estimate_gas_price` — these ARE real API routes.

The fatal part: **real routes and garbage are mixed together with no way to tell
them apart.** A buried-signal-in-noise extractor is worse than one that finds
less, because every verdict it produces is now suspect.

Root cause: `RE_ROUTE = "any quoted /path-literal"`. That matches file paths,
format strings, comments, key-derivation paths, URLs in docs — anything with a
slash in quotes.

## Evidence: recall is broken

What we DON'T extract at all, despite it being where real claims live:

1. **Usage / references.** The product's headline is "nobody uses X." Our only
   "used" signal is route-exists + log hits. We never find **call sites,
   imports, or symbol references.** "Is this function still called?" "Is this
   dependency actually imported, or just in Cargo.toml?" — unanswerable today.
2. **Framework routes.** We match quoted `/x` literals but miss the idioms that
   actually declare routes: decorators (`@GetMapping`, `@app.route`), attribute
   macros (`#[get("/x")]`), router tables, gRPC/protobuf services, OpenAPI.
3. **Feature flags** (`feature_flag_enabled`, a stated v0.1 claim type): nothing.
4. **String / boolean config** (`API_URL = "..."`, `ENABLED = true`): only
   numbers are captured.
5. **Version requirements** (`version_required`): nothing.
6. **Job schedules** (`job_last_success` — cron/schedule defs): nothing.
7. **`.env` values** (we get the var *name* via `getenv`, never the configured
   value).
8. **Test assertions** (`assert_eq!(retries, 3)` — strong intent evidence):
   ignored.
9. **Comments / docstrings** ("deprecated", "do not use", TODO): ignored — yet
   this is exactly where stale claims are written.

## Architectural ceiling

Regex-over-lines fundamentally cannot see:
- a constant's **type** or **scope** (so `MAX_RETRIES` in two modules conflate),
- whether a symbol is **public API**,
- **multi-line** definitions,
- **what calls what** (the usage graph),
- the difference between a route literal and a file path (no surrounding
  syntactic context).

The spec (§12.4) deliberately chose "simple deterministic extraction, no
complete code understanding" for the MVP. That was the right MVP call. But the
indexer is now hardened to the point where this is the binding constraint.

## Options

| Approach | Effort | Payoff |
|---|---|---|
| More/better regexes | low | precision still capped; usage still impossible |
| **Confidence + context gating** (cheap precision win) | low | only treat `/x` as a route if the line has a framework verb (`.get(`, `route`, `@`, `HandleFunc`, ...); drop bare path literals. Kills most false positives immediately |
| **Usage/reference finder** | medium | attacks the core pitch: grep-style symbol refs first, real later |
| **Tree-sitter per-language ASTs** | high | the real fix: routes by framework node, call-sites, types, scope, public API. Deterministic, fast, incremental. The Sourcegraph/GitHub approach. Removes the ceiling |

## Recommended phasing

1. **Precision gate first (cheap, immediate):** require route literals to have a
   framework signal on the line; type/scope the constants; drop single-letter and
   obvious-noise names. Turns "1,240 routes, mostly junk" into "the ~40 real
   ones." Biggest trust win per unit effort.
2. **Usage finder (medium):** symbol/import reference search so "is X used?"
   works from code.
3. **Tree-sitter spike (high):** 1-2 languages (Rust + one of TS/Python) to prove
   the AST approach, then expand. This is where "trustworthy" lives.

## Addendum: concept resolution (fuzzy vs embeddings) — measured

Resolving a fuzzy claim subject ("old checkout") to an indexed route.

| Approach | Token-overlap queries | Pure-semantic (zero overlap) | Cost | Deps |
|---|---:|---:|---|---|
| alias table | 3/6 | 0/4 | ~0 | none (manual) |
| **fuzzy (shipped)** | **6/6** | 0/4 | ~1us | **none** |
| embeddings (route paths) | — | **1/4** | model load | model2vec |
| embeddings (humanized labels) | — | **4/4** | model load | model2vec |

Findings:
- **Fuzzy handles realistic queries (6/6) for free, deterministically.** Shipped
  as the default `FuzzyResolver`.
- **Static embeddings only help if candidates are human words, not route paths.**
  Embedding `/v1/checkout` directly: 1/4. Embedding "checkout payment order":
  4/4. Route identifiers tokenize poorly for a distilled embedding model.
- Implication: embeddings are a real fallback, but they require **enriched
  concept labels** (route -> human description) to be effective. That enrichment
  (deriving "checkout payment order" from `/v1/checkout` + its handler/comments)
  is itself an extraction problem — back to needing better extraction.

`EmbeddingResolver` (model2vec, pure Rust, no candle/onnx) is implemented behind
the `embeddings` cargo feature, off by default. It is not yet wired into the
check pipeline pending the concept-enrichment work above.
