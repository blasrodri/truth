# truth

**`truth` is a deterministic fact-checker for the claims an AI coding agent makes about its own work.**

When an agent says *"I added the `/v1/refund` route, set `MAX_RETRIES` to 5, only touched the parser, and tests pass,"* `truth` checks each claim against the **real code, the working-tree git diff, recorded command runs, and logs**, and answers **Supported / Contradicted / Refused** — every verdict cited.

It is not a chatbot and it does not decide truth with a model. A language model only parses the agent's sentence into a structured claim; a fixed-rule engine decides the verdict from retrieved evidence. **The agent cannot talk it into a different answer.**

![truth catching an agent's lies](assets/demo.gif)

<sub>Reproducible: `bash examples/demo.sh` (recorded with asciinema + agg).</sub>

```
$ truth verify-turn "I added the /v1/refund endpoint, set MAX_RETRIES to 3, I only changed src/api.rs, and tests pass"

  ✓ Supported     I added the /v1/refund endpoint  (src/api.rs)
  ✗ Contradicted  set MAX_RETRIES to 3             (src/config.rs:1)
  ✗ Contradicted  I only changed src/api.rs        (src/api.rs)
  ✗ Contradicted  tests pass                       (recorded 2026-06-11 09:14 UTC)

  1 supported · 3 contradicted · 0 refused

  ⚠ The agent's message contradicts the evidence above.
```

## Why

Coding agents over-claim. They report success inferred from clean logs, invent
terminal output, and confidently describe changes they didn't make. `truth`
catches the **checkable** subset — wrong config values, routes/functions
claimed added or removed, files claimed edited, "I only changed X" scope
claims, renames, and *"tests pass"* (against recorded runs) — and **refuses**
the rest (*"this is cleaner"*) instead of guessing. A refusal is honest, not a
gap: a verifier that bluffs is worse than none.

Everything runs **locally**. The store is a single SQLite file in `.truth/`;
raw logs are never persisted (only redacted aggregates); your code never leaves
the machine. No LLM, network, or account is required.

## Install

**Claude Code — as a plugin** (hooks + MCP tool in one install, all repos):

```
/plugin marketplace add blasrodri/truth
/plugin install truth@truth
```

Then install the binary (the plugin will remind you at session start if you
skip this):

```bash
curl -fsSL https://raw.githubusercontent.com/blasrodri/truth/main/install.sh | sh
```

That's everything. **Zero per-repo setup**: the hooks fact-check any git repo
you work in — the store auto-creates (self-gitignored) on first use, the
index auto-refreshes on every check. Opt out of uninitialized repos with
`truth hook auto off`; opt a single repo into project-scoped hooks with
`truth setup`.

**Without the plugin** — the installer also registers the MCP server when the
`claude` CLI is present; add the hooks once with `truth hook install --user`
(or per repo with `truth setup`).

**Or manually** — grab the tarball for your platform from the
[latest release](https://github.com/blasrodri/truth/releases/latest)
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

> **Why no hosted option?** truth is listed on MCP registries (e.g.
> [Glama](https://glama.ai/mcp/servers)) for discovery, but it must run on
> **your** machine: the verdicts come from your working tree, your git diff,
> and your recorded test runs — none of which a remote server can see. Local
> is not a limitation; it's the product (and why your code never leaves the
> machine).

## Use it from your coding agent (MCP)

`truth-mcp` is a local [Model Context Protocol](https://modelcontextprotocol.io)
server (stdio JSON-RPC). It exposes one tool, **`verify_turn`**, that an agent
calls on **its own** message before telling you a change is done — so the agent
catches and corrects its own lies first.

The repo ships a [`server.json`](server.json) (MCP registry manifest) and a
[`.mcp.json`](.mcp.json), so cloning it auto-registers the server in MCP-aware
clients (subject to their approval prompt).

### Claude Code

`truth setup` (or `install.sh`) registers this for you. Manually:

```bash
claude mcp add --scope user truth -- truth-mcp
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
your concrete factual claims (files edited, values set, routes/functions added
or removed, renames, "only changed X", "tests pass") into the `claims` array
and pass the repo path. Run tests through `truth run -- <cmd>` so "tests pass"
is checkable. Fix anything it marks `contradicted` before reporting done.
```

Or skip the honor system entirely: `truth hook install` (next section) makes
verification run on every turn whether the agent calls the tool or not.

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

## Make it a gate the agent can't skip (hooks)

The MCP tool relies on the agent *choosing* to verify itself. Hooks remove the
choice:

```bash
truth hook install          # project .claude/settings.json (--user for global)
```

This registers two Claude Code hooks:

- **Stop** — when the agent finishes its turn, its final message is
  fact-checked against the repo, the working-tree diff, and recorded runs.
  Contradictions **block the stop** and feed the cited verdict back, so the
  agent corrects itself before you ever read the claim.
- **PostToolUse (Bash)** — test/build/lint commands the agent runs are
  recorded as command receipts automatically, which is what makes its later
  *"tests pass"* checkable. (Receipts are only recorded when the hook payload
  carries a real exit code — never guessed.)

Both hooks are fail-open: if truth errors, the session is never wedged. And
they're zero-setup: in a git repo that never ran `truth init`, the hooks
auto-create the store at the git root (self-gitignored — your diff stays
clean) and start verifying immediately. `truth hook auto off` restricts them
to explicitly initialized repos.

For pull requests, [`examples/github/pr-factcheck.yml`](examples/github/pr-factcheck.yml)
is a drop-in GitHub workflow that fact-checks a PR's **description against its
actual diff** and posts the verdict table as a comment — agent-written PRs get
checked before any human reads them.

## The lie ledger

Every check is already stored as an audit trail; `truth stats` reads it back:

```
$ truth stats --window 7d

  claims checked      142
  supported            96 (68%)
  contradicted          9 (6%)
  refused              37 (26%)
  runs recorded        12 (10 green, 2 failing)

  contradictions by claim type:
    config_value   4
    file_changed   3
```

`truth stats --all` aggregates across every repo you've run `truth init` in
(via `~/.truth/registry.json` — the stores themselves stay per-repo).

## Use it yourself (CLI)

```bash
cd your-repo
truth init                      # writes truth.toml + .truth/, runs migrations
truth index .                   # index code/docs/config (re-run after big changes)

truth verify-turn "I added /v1/refund, set MAX_RETRIES to 5, removed /v1/checkout"
truth verify-turn "<agent message>" --repo /path/to/repo --json

truth run -- cargo test         # run + record a receipt; "tests pass" becomes checkable
truth stats                     # the lie ledger
```

`--repo` opens that repo's `.truth` store and diffs that tree. Without it,
truth walks up from the current directory to the nearest `truth.toml`/`.truth`
root (like git does), so every command works from any subdirectory. `--json`
emits stable machine-readable output.

## What it can and cannot check

**Checks (state claims):** route added/removed/exists, **function/symbol
added/removed/exists**, config value, named constant ("changed X from 3 to 5"
checks the *5*), retry count, timeout value, env var present, dependency used,
version required, usage count, error-still-happening, job-last-success,
feature-flag enabled — across **Rust / TypeScript / Python / Go** — against
**code + git diff + logs**, with the diff outranking a possibly-stale index
for "I just changed X" claims.

**Checks (diff claims — what THIS turn changed):** *"I edited/created/deleted
`src/auth.rs`"* (the diff's file list decides), *"I **only** changed the
parser"* (catches collateral edits — every changed path must match), *"renamed
`parse_legacy` to `parse_v2`"* (old name must be gone AND the new one added),
*"updated all 4 call sites of X"* (changed-line count). An empty diff reports
**unknown (already committed?)** — never a free pass.

**Checks (command receipts):** *"tests pass"*, *"it compiles"*, *"clippy is
clean"* — verified against runs recorded by `truth run -- cargo test` (or the
Claude Code hook below). Supported **only** when a matching run exited 0
**after** your last working-tree edit; a failing receipt contradicts the
claim; a green-but-stale receipt proves nothing and is refused.

**Refuses (by design):** action claims with no receipt (*"I ran the tests"* —
record runs and it becomes checkable), and judgment claims (*"this is cleaner
/ faster"* — no measurable subject). Refused ≠ confirmed.

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

The highest-value knob is **`indexer.extractor`**: `mixed` (the default —
AST-precise function/struct/route definitions for Rust, TypeScript/JavaScript,
Python, and Go, so a symbol named only in a comment isn't mistaken for a real
definition; regex fills in everywhere else) · `ast` · `regex` (fastest,
language-agnostic, noisier). Re-run `truth index .` after changing it.

## How it works

```
agent message
  → segment into candidate claims (sentences, clauses)
  → claim extraction (regex by default; optional LLM, never decides truth)
  → structured claim  (unverifiable → Refused, never guessed)
  → query plan (safe templates only — the LLM never writes LogQL/SQL)
  → evidence: repo index + working-tree git diff (changed lines, file list,
    untracked files) + command receipts (`truth run`) + log queries
  → deterministic verdict engine (fixed rules, source-authority order,
    diff > stale index, receipts must postdate the last edit)
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
run       Run a command and record a receipt (makes "tests pass" verifiable)
record-run  Record a receipt for a command that already ran (hooks, CI)
stats     The lie ledger: claims checked, contradictions caught, runs recorded
hook      Install agent-harness hooks (verification the agent can't skip)
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

Covers extractors (Rust/TS/Python/Go routes, constants, env vars, deps, file/
scope/rename/count/command claims), the git-diff adapter (changed lines, file
statuses, untracked files), the verdict rules and golden fixtures (including
receipt freshness: green-but-stale runs never pass), claim segmentation, hook
settings merging, index-freshness warnings, JSON output, and end-to-end checks
over the sample repo. `truth eval fixtures/eval/agent_claims.yaml` is the
agent-fact-checking quality harness.

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
