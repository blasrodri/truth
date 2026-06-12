# Walkthrough: catch an agent lying about this repo

This is a 2-minute tour of `truth` using the tiny repo in this folder. Every
command and every line of output below is **real** — copy/paste and you'll see
the same verdicts. (The recorded version is the GIF in the README; this is the
same story you can run by hand.)

## The repo

[`examples/sample-repo`](.) is a deliberately small service with a known ground
truth — and one planted lie:

| Fact | Where | Value |
|---|---|---|
| payment retry count | `src/routes/checkout.rs` | **5** (`MAX_RETRIES` / `PAYMENT_RETRY_COUNT`) |
| request timeout | `src/routes/checkout.rs` | **30** seconds |
| bind port | `src/config.toml` | **8080** |
| `/v1/checkout` route | `src/routes/checkout.rs` | exists (POST + GET) |
| dependencies | `package.json` | `express`, `stripe`, `jest` |
| `docs/payments.md` | docs | says "3 retries" — **stale on purpose**; the code says 5 |

The point of the stale doc: an agent that reads the docs and reports "we retry 3
times" is confidently wrong, and `truth` should catch it against the code.

## 1. Set it up

```bash
# copy the sample repo somewhere writable and make it a git repo
cp -r examples/sample-repo /tmp/demo && cd /tmp/demo
git init -q . && git add -A && git commit -qm baseline

truth init        # writes truth.toml + .truth/ (both self-gitignored)
truth index .     # index code/docs/config
```

```
Wrote truth.toml
Added to .gitignore: .truth/, truth.toml
Initialized database at /tmp/demo/.truth/truth.sqlite
Indexed 5 files → 5 artifacts, 20 evidence items (extractor: mixed).
```

## 2. Fact-check a mixed agent message

Now play the agent. Two of these claims are true, one is a lie:

```bash
truth verify-turn "I set the payment retry count to 5, the service runs on \
  port 8080, and I lowered the request timeout to 10 seconds" --repo /tmp/demo
```

```
  ✓ Supported     I set the payment retry count to 5  (/tmp/demo/src/routes/checkout.rs:4)
  ✓ Supported     the service runs on port 8080  (/tmp/demo/src/config.toml:3)
  ✗ Contradicted  I lowered the request timeout to 10 seconds  (/tmp/demo/src/routes/checkout.rs:10)

  2 supported · 1 contradicted · 0 refused

  ⚠ The agent's message contradicts the evidence above.
```

The timeout claim is caught: the code says 30, not 10. **Each verdict cites the
exact file and line** it was decided from — no model opinion involved.

## 3. Catch a scope lie ("I only changed X")

Make a real edit to **two** files, then claim you only touched one:

```bash
printf '\npub fn new_helper() {}\n' >> src/routes/checkout.rs
echo '# touched' >> src/config.toml

truth verify-turn "I only changed src/routes/checkout.rs this turn" --repo /tmp/demo
```

```
  ✗ Contradicted  I only changed src/routes/checkout.rs this turn  (/tmp/demo/src/routes/checkout.rs)

  0 supported · 1 contradicted · 0 refused

  ⚠ The agent's message contradicts the evidence above.
```

The working-tree diff shows `src/config.toml` changed too, so the "only" claim
is contradicted — collateral edits don't slip through.

## 4. Make "tests pass" checkable

A bare *"tests pass"* with no recorded run is **Refused**, not trusted. Record a
run and it becomes checkable. First a failing one:

```bash
truth run -- sh -c 'echo "running tests"; exit 1'    # (use your real test cmd)
truth verify-turn "tests pass" --repo /tmp/demo
```

```
truth: recorded `sh -c echo "running tests"; exit 1` → exit 1 (test receipt)

  ✗ Contradicted  tests pass  (recorded 2026-06-12 22:10 UTC)
```

Then a passing one:

```bash
truth run -- sh -c 'echo "test result: ok"; exit 0'
truth verify-turn "tests pass" --repo /tmp/demo
```

```
truth: recorded `sh -c echo "test result: ok"; exit 0` → exit 0 (test receipt)

  ✓ Supported     tests pass  (recorded 2026-06-12 22:10 UTC)
```

A green receipt only counts when it exited 0 **after** your last edit — a
green-but-stale run proves nothing and is refused (see
[`docs/THREAT_MODEL.md`](../../docs/THREAT_MODEL.md)).

## 5. Read the ledger

Every check is stored. `truth stats` reads the audit trail back:

```
  claims checked        5
  supported             2 (40%)
  contradicted          2 (40%)
  refused               1 (20%)
  runs recorded         1 (0 green, 1 failing)

  contradictions by claim type:
    only_changed       1
    timeout_value      1
```

## What just happened

Three different kinds of lie — a wrong **value**, a wrong **scope**, and an
unbacked **"tests pass"** — were each caught against a *different* kind of
evidence (the indexed code, the git diff, a recorded run), and the true claims
were Supported with citations. No model decided any verdict; a fixed-rule engine
did. That's the whole product.

Next: wire it into your agent so this runs on every turn — see the README
[*"Make it a gate"*](../../README.md#make-it-a-gate-the-agent-cant-skip-hooks)
section.
