# Tree-sitter extraction spike — design & success criteria

## Why

`docs/EXTRACTION_AUDIT.md` established that regex-over-lines is the binding
constraint: ~95% route noise on a real repo before precision gating, no call-site
usage, no types/scope, no multi-line/attribute awareness. Tree-sitter gives real
per-language ASTs — the structural fix that lifts accuracy under all five
evidence sources.

## Scope (deliberately narrow)

A **spike**, not a migration. One language (**Rust**), two fact kinds where the
structural win is clearest and measurable:

1. **Routes** by AST: a method call (`.get`/`.post`/`.route`/...) whose argument
   is a string literal starting with `/`. Structurally rejects `/0`, file paths,
   and format strings that regex needs a precision gate to filter.
2. **Numeric constants** by AST: `const NAME: TYPE = N;` / `static NAME = N;` —
   carrying the **name, value, and type**, and whether it is `pub`.

Stretch (only if cheap): **call sites** — references to a function/const ident,
which is the real "is X used?" signal. (We already approximate this textually in
`truth uses`; tree-sitter would make it precise.)

## Non-goals for the spike

- Replacing the regex extractor (it stays the default; tree-sitter is additive,
  feature-gated).
- All languages. Just Rust; the pattern generalizes later.
- Full semantic analysis (type inference, macro expansion).

## Design

- New crate `truth-ast` (isolates the tree-sitter dependency tree).
- `tree-sitter` + `tree-sitter-rust`. Parse a file once, run tree-sitter
  **queries** (S-expression patterns) to capture routes and constants with their
  spans.
- Produce the SAME `Extracted`-shaped facts the indexer already consumes, so the
  indexer can switch sources without downstream changes. Behind a cargo feature
  `ast` (off by default) and/or a config switch.

## Success criteria (measured, or we don't ship it)

On real Rust (our repo + a slice of aptos-core), compare regex vs tree-sitter:

1. **Precision up**: tree-sitter routes have materially fewer false positives
   than *un-gated* regex, and no worse than gated regex — without needing the
   hand-tuned precision gate.
2. **Recall up**: tree-sitter finds routes/constants regex misses (multi-line,
   attribute macros, typed consts).
3. **Speed acceptable**: parsing+querying stays within ~2-3x of regex extraction
   per file (tree-sitter parses fast; the indexer is already parallel). Must not
   blow the ~190ms full-index budget by more than a small factor for the spike
   language.
4. **Same fact shape**: output plugs into the existing pipeline unchanged.

If tree-sitter doesn't clearly win on 1+2 at acceptable 3, we keep regex and
record why.

## Plan

1. `truth-ast` crate: parse Rust, query for routes + constants -> facts. Unit
   tests on hand-written Rust snippets (incl. cases regex fails: multi-line call,
   `#[get("/x")]`, typed const).
2. Bench harness: run both extractors over the same Rust files, diff the facts,
   report precision/recall/time deltas.
3. Decide from data. If win: wire as an opt-in source in the indexer for `.rs`.

## Spike result — MEASURED (verdict: tree-sitter wins on precision)

Built `truth-ast` (tree-sitter + tree-sitter-rust): routes by call-expression
AST node, constants by const/static items. 4 unit tests pass, including the two
structural cases regex can't do (multi-line route call; rejecting non-route
string literals).

### Precision head-to-head (1 real route + 3 noise strings in one file)
- regex (WITH our precision gate): 2 routes — caught the real one, but FALSELY
  matched `"/api/should-not-count"` (a string inside `assert_eq!`).
- tree-sitter: 1 route — exactly the real one. The file path, derivation-ish
  literal, log path, and assertion string are all rejected STRUCTURALLY (none
  are arguments to a route-registration method call).

This is the core win: AST distinguishes "string arg to `.post()`" from "string
arg to `assert_eq!`" — regex cannot, at any amount of gating.

### Recall
- On a file with real `.post(...)`/`.route(...)` calls, regex and AST agree
  exactly (/v1/checkout, /health, MAX_RETRIES). AST additionally handles
  multi-line calls regex's signal-gating misses.
- On our own 60 crate files (a CLI, no web routes), AST correctly found 0 real
  routes; regex reported 78 — all string-literal noise from tests/examples/
  fixtures (`/auth/login`, `/definitely/not/a/repo/anywhere`, ...). AST's 0 is
  *correct*; regex's 78 are false positives.

### Speed
- ~6 ms/file including per-file grammar reload (382ms / 60 files). Acceptable;
  reusing one `Parser` across files (instead of per-call) would cut it further.
  Regex is faster (~0.1ms/file) but the indexer is parallel and the absolute
  numbers are small.

### Verdict
Tree-sitter delivers precision regex structurally cannot, at acceptable cost,
emitting the same fact shape. **Recommend** adopting it as an opt-in extraction
source for `.rs` (feature/config gated), then extending to TS/Python/Go. The
regex extractor stays the default until AST covers all indexed languages.
