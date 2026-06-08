# truth

**`truth` is a deterministic fact-checker for the claims an AI coding agent makes about its own work.**

When an agent says *"I added the `/v1/refund` route, set `MAX_RETRIES` to 5, and removed the old checkout handler,"* `truth` checks each claim against the **real code, the working-tree git diff, and logs**, and answers **Supported / Contradicted / Refused** — every verdict cited.

It is not a chatbot and it does not decide truth with a model. A language model only parses the agent's sentence into a structured claim; a fixed-rule engine decides the verdict from retrieved evidence. **The agent cannot talk it into a different answer.**

```
$ truth verify-turn "I added the /v1/refund endpoint, set MAX_RETRIES to 3, and it runs on port 8080"

  ✓ Supported     I added the /v1/refund endpoint  (src/config.rs)
  ✗ Contradicted  set MAX_RETRIES to 3             (src/config.rs:1)
  ✓ Supported     it runs on port 8080            (config.toml:1)

  2 supported · 1 contradicted · 0 refused

  ⚠ The agent's message contradicts the evidence above.
```

## Why

Coding agents over-claim. They report success inferred from clean logs, invent
terminal output, and confidently describe changes they didn't make. `truth`
catches the **checkable** subset — wrong config values, routes claimed
added/removed, things claimed unused — and **refuses** the rest (*"tests pass"*,
*"this is cleaner"*) instead of guessing. A refusal is honest, not a gap: a
verifier that bluffs is worse than none.

Everything runs **locally**. The store is a single SQLite file in `.truth/`;
raw logs are never persisted (only redacted aggregates); your code never leaves
the machine. No LLM, network, or account is required.

## Install

**Prebuilt binaries** (no Rust toolchain needed) — grab the tarball for your
platform from the [latest release](https://github.com/blasrodri/truth/releases/latest)
(macOS arm64/x64, Linux x64/arm64), then:

```bash
tar -xzf truth-*.tar.gz
install truth-*/truth truth-*/truth-mcp /usr/local/bin/
```

**Or build from source:**

```bash
cargo build --release --workspace
install target/release/truth target/release/truth-mcp /usr/local/bin/
# binaries: truth (CLI) and truth-mcp (the MCP server)
```

## Use it from your coding agent (MCP)

`truth-mcp` is a local [Model Context Protocol](https://modelcontextprotocol.io)
server (stdio JSON-RPC). It exposes one tool, **`verify_turn`**, that an agent
calls on **its own** message before telling you a change is done — so the agent
catches and corrects its own lies first.

The repo ships a [`server.json`](server.json) (MCP registry manifest) and a
[`.mcp.json`](.mcp.json), so cloning it auto-registers the server in MCP-aware
clients (subject to their approval prompt).

### Claude Code

```bash
claude mcp add --scope user truth -- /usr/local/bin/truth-mcp
claude mcp list          # verify it connected
```

### Cursor, or any MCP client (generic config)

Add to `~/.cursor/mcp.json` (Cursor) or your client's `mcpServers` block:

```json
{
  "mcpServers": {
    "truth": {
      "type": "stdio",
      "command": "/usr/local/bin/truth-mcp"
    }
  }
}
```

That's the whole config — **no per-repo setup in the MCP file**. The agent
passes the repo path with each call (the `repo` argument below), so one
registration works across all your projects.

### The `verify_turn` tool

| Argument | Required | Meaning |
|---|---|---|
| `message` | yes | The agent's raw prose about its work. truth scans it as a backstop, so claims you forget to list in `claims` still get checked. |
| `claims` | recommended | An array of the individual factual claims, each a short self-contained sentence (`["I set MAX_RETRIES to 5", "I added the /v1/refund route"]`). The **calling model extracts these from its own message** — it's free (the agent is already mid-turn) and far more reliable than truth re-parsing prose. truth still decides each verdict from real evidence, not from the wording. |
| `repo` | recommended | Absolute path to the repo root. `truth` opens `<repo>/.truth` and diffs that working tree. Omit it and the server falls back to its own working directory, which may be wrong. |
| `local_log` | no | Path to a local log file for usage/error claims. |

> **Why `claims` is the elegant path:** truth keeps the hybrid architecture —
> the LLM *parses*, the deterministic engine *decides*. By having the agent
> (already an LLM, already mid-turn) extract its own claims, truth sidesteps its
> regex parser entirely for ~tens of tokens, while still never letting a model
> decide truth. Omissions are caught by the `message` backstop.

**Guaranteeing the agent uses `claims`.** The tool description asks for it, but
to make it a habit in your own repos, add a line to your agent's project
instructions (e.g. `CLAUDE.md`):

```md
Before telling me a code change is done, call the `verify_turn` tool: extract
your concrete factual claims (values set, routes/functions added or removed)
into the `claims` array and pass the repo path. Fix anything it marks
`contradicted` before reporting done.
```

It returns the verdict table as text plus `structuredContent` (the JSON below),
including an `index` block reporting whether the index is empty or stale — so a
"clean" result is never trusted blindly.

> **One-time per repo:** run `truth init` once to create the `.truth/` store.
> After that, `verify_turn` **auto-refreshes the index** on every call —
> incrementally, skipping unchanged files (~10–50 ms), so code-existence /
> usage / config claims always reflect the current working tree. You never have
> to re-run `truth index` by hand. Claims about the **working-tree diff**
> ("I added/removed X") need no index at all. If the index still ends up empty
> (e.g. nothing indexable), `verify_turn` says so loudly instead of passing.

## Use it yourself (CLI)

```bash
cd your-repo
truth init                      # writes truth.toml + .truth/, runs migrations
truth index .                   # index code/docs/config (re-run after big changes)

truth verify-turn "I added /v1/refund, set MAX_RETRIES to 5, removed /v1/checkout"
truth verify-turn "<agent message>" --repo /path/to/repo --json
```

`--repo` opens that repo's `.truth` store and diffs that tree (don't rely on the
process working directory). `--json` emits stable machine-readable output.

## What it can and cannot check

**Checks (state claims):** route added/removed/exists, **function/symbol
added/removed/exists**, config value, named constant, retry count, timeout
value, env var present, dependency used, version required, usage count,
error-still-happening, job-last-success, feature-flag enabled — across
**Rust / TypeScript / Python / Go** — against **code + git diff + logs**, with
the diff outranking a possibly-stale index for "I just changed X" claims.

**Refuses (by design):** action claims (*"I ran the tests"* — no evidence
source for *I ran*), and judgment claims (*"this is cleaner / faster"* — no
measurable subject). Refused ≠ confirmed.

## Configuration

Behavior lives in `truth.toml` (written by `truth init`). Tweak it without
hand-editing — `truth settings` validates and preserves the rest of the file,
so a user *or an agent* can change knobs programmatically:

```bash
truth settings list                              # every knob, current value, help
truth settings set indexer.extractor mixed       # turn on AST precision (symbols/routes)
truth settings set repo.include src,lib,app      # what to index
truth settings set llm.enabled true              # use an LLM to parse claims (engine still decides)
truth settings get indexer.extractor --json
```

The highest-value knob is **`indexer.extractor`**: `regex` (default, fast) ·
`ast` · `mixed` (AST-precise function/struct/route definitions for Rust, so a
symbol named only in a comment isn't mistaken for a real definition). Re-run
`truth index .` after changing it.

## How it works

```
agent message
  → segment into candidate claims (sentences, clauses)
  → claim extraction (regex by default; optional LLM, never decides truth)
  → structured claim  (unverifiable → Refused, never guessed)
  → query plan (safe templates only — the LLM never writes LogQL/SQL)
  → evidence: repo index + working-tree git diff + log queries
  → deterministic verdict engine (fixed rules, source-authority order, diff > stale index)
  → cited verdict: Supported / Contradicted / Refused
```

Every check is stored as an audit trail (the claim, the queries run, the
verdict) in SQLite. Log samples are redacted (emails, JWTs, UUIDs, IPs, tokens)
before being stored or shown.

## Other commands

`truth` is built on a general claim/evidence engine; `verify-turn` is the agent
front door. The engine is also usable directly:

```
check     Check a single natural-language engineering claim
usage     Observed usage of a route/event/pattern (deterministic)
errors    Error occurrences (deterministic)
config    Search indexed config/code definitions
owners    Who has worked on the code behind a subject
uses      Find code references to a symbol/route/dependency
docs      Is a subject documented, and consistent with code?
inspect   Show exactly what was indexed (trust the evidence)
doctor    Validate local setup and explain readiness
claims/report/ci/eval/diff   claim files, reports, CI gates, regression diffs
```

Run `truth <command> --help` for details. `truth serve` (Slack/HTTP) is an
informational placeholder and intentionally not built — the local verifier is
the product.

## Tests

```bash
cargo test --workspace
```

Covers extractors (Rust/TS/Python/Go routes, constants, env vars, deps), the
git-diff adapter, the verdict rules and golden fixtures, claim segmentation,
index-freshness warnings, JSON output, and end-to-end checks over the sample
repo. `truth eval fixtures/eval/agent_claims.yaml` is the agent-fact-checking
quality harness.

### Measuring extraction quality

`fixtures/eval/extractor_corpus.yaml` is a **diagnostic corpus**, not a gate: the
same ground-truth facts phrased many ways, including hard `H*` edge cases the
regex extractor is *expected* to miss. Run it to measure where claim extraction
stands and what a better extractor (agent-supplied `claims`, a local LLM, or
AST) would improve:

```bash
truth eval fixtures/eval/extractor_corpus.yaml
```

A `T*`/`H*` case that returns `inconclusive` is a **recall gap** (extractor too
weak); an `F*` case that returns `supported` is a dangerous **false pass**; an
`R*` case that returns a verdict is a **hallucination**. The bands make all
three visible, so changes can be measured instead of guessed.
