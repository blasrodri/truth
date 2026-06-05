# truth

`truth` is a Slack-native engineering claim checker.

When someone says "nobody uses this endpoint," "the issue is fixed," or "we still
retry 3 times," `truth` checks the claim against code, docs, config, logs, and
metrics, then replies with a cited verdict.

It is not a chatbot. It is a conservative evidence engine for engineering teams.

## Status

This build implements **v0.1 Phases 1–4** plus a hardened deterministic CLI
(core → LLM → repo indexer → Loki, with explicit observation commands, JSON
output, an audit-trail explainer, and an evaluation harness). The Slack bot and
HTTP server are intentionally **not** included; `truth serve` is a placeholder.

The whole pipeline runs **offline**: claim extraction falls back to a
deterministic regex extractor when no LLM is configured, and a local log file
adapter substitutes for Loki. No LLM, network, or Slack credentials are required.

## Layout

```
crates/
  truth-core/     domain models, enums, config, verdict engine, adapter traits
  truth-db/       rusqlite migrations + persistence
  truth-indexer/  repo walker + deterministic evidence extractors
  truth-logs/     Loki adapter, local-file adapter, PII redaction
  truth-llm/      regex claim extractor (default) + optional OpenAI-compatible client,
                  query planner, response generator
  truth-cli/      the `truth` binary and command implementations
migrations/       SQLite schema
examples/         sample-repo and sample-logs for the demo
fixtures/eval/    evaluation fixtures (the quality harness)
```

## Commands

```
init       Initialize truth config and database
index      Index repo docs/code/config
doctor     Validate local setup and explain readiness
inspect    Show what was indexed (routes/constants/env/ports/deps/evidence)
baseline   Run auto-generated checks from indexed evidence + logs
check      Check a natural-language engineering claim
usage      Check observed usage of a route/event/pattern
errors     Check error occurrences
latest     Find latest occurrence of a pattern
config     Search indexed config/code definitions
explain    Explain a previous check from the audit trail
eval       Run an evaluation fixture (YAML); --record captures a baseline
claims     Extract candidate claims from docs/text into a claim file
report     Run checks from a claim file and produce a report
ci         Run checks from a claim file and exit per a fail policy
diff       Compare two reports or recorded outputs
db         Database commands (migrate)
serve      Placeholder for future Slack/server mode
```

`check` is natural-language oriented (uses the LLM if configured, else regex).
`usage` / `errors` / `latest` / `config` are **deterministic** — they take an
explicit subject and never invoke the LLM. They report observations
(`Observed` / `Not observed` / `Inconclusive`), not claim verdicts.

`doctor` reports whether `truth` is configured and indexed, including Loki/LLM
reachability. `inspect` shows exactly what was indexed (so you can trust the
evidence). `baseline` auto-generates `usage`/`config` checks from the index and
is purely observational — it never fails because errors were observed.

`--json` is available on `check`, `usage`, `errors`, `latest`, `config`,
`explain`, `doctor`, `inspect`, and `baseline`, and emits stable machine-readable
JSON with no extra prose.

`--local-log <path>` forces the offline local-file log adapter and always takes
precedence over Loki, so the demo works regardless of `[loki] enabled`.

`truth eval <fixture> --record <out.yaml>` captures actual outputs as a recorded
baseline fixture (refuses to overwrite without `--force`) — useful for building a
regression corpus from real repos.

## Claim files, reports, and CI

A **claim file** is a reviewable YAML list of engineering claims that powers
`report`, `ci`, and `eval`:

```yaml
version: 1
defaults:
  repo: .
  local_log: examples/sample-logs/api.log
  window: 7d
claims:
  - id: checkout-unused
    text: nobody uses /v1/checkout anymore
    severity: warning          # info | warning | error
    tags: [usage, checkout]
    expected_status: contradicted   # optional; used by eval
```

Generate a starter claim file from your docs (deterministic; LLM optional, never
required), then **review and edit it by hand**:

```bash
truth claims README.md docs/ --out .truth/claims.yaml
```

Run the claims and produce a report (text / markdown / json). `report` never
fails because a claim is contradicted — it is a reporting command (exit 0 unless
something operational breaks):

```bash
truth report .truth/claims.yaml --local-log app.log --format markdown --out truth-report.md
truth report .truth/claims.yaml --local-log app.log --format json     --out truth-report.json
```

Gate CI on a policy. Exit codes: `0` pass, `1` policy failed, `2` operational
error. The default only fails on `contradicted` claims with `severity: error`,
so adoption is painless; tighten it as you go:

```bash
truth ci .truth/claims.yaml --local-log app.log --fail-on contradicted --fail-severity warning
```

Track regressions between two runs (report JSON or recorded eval YAML):

```bash
truth diff old-report.json new-report.json
truth diff old-report.json new-report.json --json
```

## Dogfooding loop

```bash
truth init
truth index .
truth inspect
truth claims README.md docs/ --out .truth/claims.yaml
# manually review / edit .truth/claims.yaml
truth report .truth/claims.yaml --local-log path/to/log --format markdown --out truth-report.md
truth ci .truth/claims.yaml --local-log path/to/log --fail-on contradicted --fail-severity warning
```

No Slack and no LLM are needed. Claim files are meant to be reviewed and
committed. Reports are plain markdown/JSON suitable for CI artifacts or (later)
PR comments.

## Quick start (offline demo)

```bash
cargo build --workspace

# In a scratch directory:
truth init                 # writes truth.toml + .truth/, runs migrations
truth index path/to/examples/sample-repo
LOG=path/to/examples/sample-logs/api.log

truth check  "nobody uses /v1/checkout anymore" --local-log $LOG   # → Contradicted
truth check  "we retry payments 3 times"        --local-log $LOG   # → Contradicted
truth check  "the service runs on port 8080"    --local-log $LOG   # → Supported

truth usage  /v1/checkout                        --local-log $LOG  # → Observed (4 requests)
truth errors webhook_signature_failed            --local-log $LOG  # → error occurrences
truth latest /v1/checkout                        --local-log $LOG  # → latest timestamp
truth config MAX_RETRIES                                            # → repo definition (=5)
truth eval   fixtures/eval/basic.yaml                               # → quality harness
```

The local log file may be plain text or JSON-lines; JSON entries are matched on
structured fields (`route`/`path` for usage, `error`/`message` for errors).

## Real-world first run

On your own repository:

```bash
truth init
truth doctor                                  # is truth configured and ready?
truth index .
truth inspect                                  # what did truth find?
truth baseline --local-log path/to/log         # auto-checks from the index
truth usage /your/route --local-log path/to/log
truth check "nobody uses /your/route anymore" --local-log path/to/log
truth eval fixtures/eval/basic.yaml
```

`doctor` tells you whether setup is ready and what to run next. `inspect` lets
you confirm `truth` actually found the routes, constants, env vars, and
dependencies you care about before you trust a verdict. If a `check` comes back
`Inconclusive`, it explains the likely cause (not indexed / no log source / the
subject couldn't be resolved) and suggests the next command.

Notes:

- **No Slack and no HTTP server yet** — the core verifier is the product. `truth
  serve` is an informational placeholder.
- **No LLM required.** Claim extraction falls back to a deterministic regex
  extractor; everything runs offline with no network access.
- **Local logs work offline** via `--local-log`, which always overrides Loki.
- **Raw logs are never stored.** Only query text, aggregate counts, the latest
  timestamp, and a few **redacted** samples are persisted.
- **Samples are redacted** (emails, JWTs, UUIDs, IPs, tokens) before being
  stored or shown.

## How it works

```
claim text
  → claim extraction (regex by default, LLM optional)
  → structured claim
  → query plan (safe templates only — the LLM never writes LogQL/SQL)
  → deterministic source adapters (repo evidence + log queries)
  → verdict engine (fixed rules + source-authority ordering)
  → response generator (structured data only; raw logs never surfaced)
```

Every check stores an audit trail (the check, the queries run, the verdict) in
SQLite. Log samples are redacted (emails, JWTs, UUIDs, IPs, tokens) before being
stored or shown.

## Configuration

See `truth.toml.example`. `truth init` copies it to `truth.toml`.

## Tests

```bash
cargo test --workspace
```

Covers enum/DB round-trips, migrations, multi-language extractors (Rust / TS /
Python / Go routes, constants, env vars, deps), JSON-lines + plain-text log
parsing, LogQL generation, PII redaction, the verdict rules, golden verdict
fixtures, the deterministic observation commands, JSON output, the eval harness,
and an end-to-end check over the sample repo + logs.

`truth eval fixtures/eval/basic.yaml` is the product's quality harness: each
case indexes a repo into a fresh in-memory DB, runs a check, and asserts the
verdict status. It exits non-zero if any case fails.
