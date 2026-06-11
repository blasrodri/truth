# Design note: reading git history as an evidence source

> **Status update (2026-06):** Phase 1 (recency) is built — `truth-git`'s
> `GitHistory::last_modified` is attached lazily at check time (see
> `git_recency_for` in `truth-cli/src/check.rs`), shelling out with graceful
> no-git degradation as proposed below. Separately — and distinct from
> *history* — the **working-tree diff** is now a first-class evidence source
> (`truth-cli/src/diff_facts.rs`): changed lines, the `--name-status` file
> list (rename-aware, untracked files included), powering the
> FileChanged/OnlyChanged/ChangeCount/SymbolRenamed claim types. Phases 2–4
> (commit-message grep, targeted blame, deletion detection) remain open.

## Status quo (at time of writing)

`truth` reads only the **working-tree snapshot** at index time. It never reads
git history. Evidence of this:
- The walker *excludes* `.git/`; no code opens a repository.
- Every `GitRepo` artifact is written with `authored_at: None`, `author: None`.
- The data model is already git-aware and unused: `EvidenceType::{Change,
  Decision}`, `ArtifactKind::{Commit, PullRequest, Issue}`, `Authority::{Adr,
  PullRequest, Issue}`, and the `authored_at`/`author` columns.

So we are a fact-checker that sees *what the code is*, never *how it got that
way* — which is some of the strongest evidence for the product's own questions.

## Why it matters (per claim type)

| Claim | Git evidence we're missing |
|---|---|
| "nobody uses /v1/checkout anymore" | last-modified date of the route's file; whether the handler was deleted in a recent commit |
| "this is deprecated" | the commit + author + message that deprecated it |
| "webhook errors are fixed" | a recent `fix: webhook ...` commit (Decision evidence) |
| "we retry 3 times" | `git blame` on the constant: when it changed from 3→5, by whom |

The spec's §13.2 ranks historical-decision authority as `ADR > PR > Issue >
Slack > Code` — unreachable today because we produce no PR/commit evidence.

## Measured cost (this is the deciding factor)

On **aptos-core (24,632 commits)**, cold-ish then warm:

| Operation | Cost | Notes |
|---|---|---|
| `git log -1 --format=%ct -- <file>` (last-modified) | 60ms cold / 20ms warm | per file |
| `git blame --line-porcelain <file>` | ~290ms | per file, the expensive one |
| `git log --grep=<pat>` (commit-message search) | ~10ms | whole history, cheap |

On `truth` itself (12 commits): all operations <10ms.

Conclusions:
- **Per-check, lazy is the right model.** A check touches a handful of subjects,
  not 6,000 files. 1–3 `git log`/`blame` calls per check = tens to low-hundreds
  of ms. Acceptable for a check (which already does LLM/log work); unacceptable
  if done for every file at index time.
- **`blame` is 5–10× costlier than `log -1`.** Use `log -1` (recency) eagerly;
  reserve `blame` for value claims where "who/when changed this" is the point.
- **commit-message grep is nearly free** — great for incident/deprecation claims.
- Do NOT bulk-populate `authored_at` for all artifacts at index time: that's
  6,000 × 60ms ≈ 6 minutes, destroying the ~0.2s index. Recency is a *check-time*
  lookup, not an index-time field — or an opt-in `--git-meta` pass.

## Implementation choice: shell out vs. pure-Rust

- **`gix`** (pure Rust, no C) fits the lean-binary ethos but is a large dep tree
  and `blame`/`log` ergonomics are still maturing.
- **Shell to `git`** is trivial, fast (numbers above are the system `git`),
  needs `git` on PATH, and is what most tools do. Given the measured speed and
  zero added dependency, **shell out**, behind a small `GitHistory` abstraction
  so we can swap to `gix` later.

Degrade gracefully: if `.git` is absent or `git` errors, return no git evidence
(never fail the check). Detect once per repo.

## Proposed phasing

1. **Recency (cheapest, highest signal/cost):** a `GitHistory::last_modified(
   path) -> Option<date>` via `git log -1`. Attach to usage/error verdicts: "and
   the file hasn't changed since 2023-11" strengthens "nobody uses it". Surfaced
   as a caveat/evidence line, `EvidenceType::Observation`.
2. **Commit-message evidence (cheap):** for incident-status / deprecation claims,
   `git log --grep` on the subject + {fix,deprecate,remove}; surface top matches
   as `EvidenceType::Decision` with sha/author/date.
3. **Blame (targeted):** for value claims (`retry_count == 3`), blame the
   constant's line to cite who/when set the current value, as
   `EvidenceType::Change`. Only when the verdict hinges on it.
4. **(Later) deletion detection:** when an indexed route is gone from HEAD,
   `git log --diff-filter=D -- <path>` to say "removed in commit X" — devastating
   evidence for "nobody uses it".

## Non-goals (now)

- Bulk index-time population of author/authored_at (cost above).
- GitHub PR/issue API (the spec's later phase; needs network + auth).
- gix migration (revisit if `git` dependency proves a problem).

## Verdict

Phase 1 (recency) is cheap, deterministic, offline, high-signal, and needs no new
dependency — clear first step. Phases 2–3 are similarly bounded. Recommend
building behind a `GitHistory` abstraction with graceful no-git degradation, all
check-time and lazy.
