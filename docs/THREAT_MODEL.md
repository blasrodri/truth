# Threat model

`truth` exists to make a coding agent's claims about its own work checkable
instead of taken on faith. Because the product *is* trust, it has to be honest
about its own boundaries. This document states plainly what a verdict proves,
what it does not, and how a motivated (or merely careless) agent could still get
a false claim past it.

## What truth is trying to defend against

The adversary is an **over-claiming coding agent** — usually not malicious, just
optimistic. It reports success it inferred rather than observed: it says a
config value changed when it edited the wrong file, claims "tests pass" from a
clean-looking log, says it "only touched the parser" while leaving collateral
edits, or describes a route it never wired up. `truth` turns the checkable
subset of those statements into **Supported / Contradicted / Refused** verdicts,
each tied to evidence.

It is **not** designed to defend against a hostile human who controls the
machine. Anyone who can write your working tree, your git history, your
`.truth/` store, or your `truth` binary can manufacture any verdict they like.
That is out of scope (see "Trust boundaries" below).

## What a verdict can prove

A **Contradicted** verdict is the strong one. It means the agent's claim
disagrees with primary evidence that `truth` read directly:

- **Config / constant / value claims** — the named constant, retry count,
  timeout, port, env var, or version actually present in the indexed working
  tree differs from the value claimed. ("Set `MAX_RETRIES` to 5" vs. the source
  that says `3`.)
- **Code-existence claims** — a route, function, symbol, or dependency claimed
  to exist (or to be gone) does or doesn't exist in the current tree.
- **Diff claims** — what *this turn* changed, decided by the working-tree git
  diff: the file list ("I edited `auth.rs`"), scope ("I **only** changed the
  parser" — every changed path must match), renames (old name gone **and** new
  name added), and changed-line counts.
- **Command-receipt claims** — "tests pass", "it compiles", "clippy is clean"
  are Supported **only** when a recorded run exited `0` **after** your last
  working-tree edit. A failing receipt contradicts; a green-but-*stale* receipt
  proves nothing and is **Refused**, not passed.

A **Supported** verdict is weaker than a Contradicted one, and deliberately so.
It means "the evidence `truth` retrieved is consistent with the claim," not
"this claim is true in every sense." The asymmetry is intentional: the tool is
built to catch lies, and it is far more confident saying *this disagrees with
the code* than *this is the whole truth*.

A **Refused** verdict is a feature, not a gap. For action claims with no receipt
("I ran the tests") and judgment claims ("this is cleaner / faster"), `truth`
declines to guess. **Refused ≠ confirmed.** A verifier that bluffs is worse than
none.

## What a verdict cannot prove

- **It does not prove the code is correct.** "Supported: `MAX_RETRIES` is 5"
  says the constant is 5, not that 5 is the right number or that the retry logic
  works.
- **It does not prove behavior at runtime.** It reads source, diffs, receipts,
  and logs — it does not execute your program or reason about what the code
  *does* when run.
- **It does not prove semantic intent.** A route can exist at the claimed path
  and still do the wrong thing; a renamed symbol can be renamed everywhere and
  still be a bad rename.
- **It does not prove anything about claims it Refused.** A turn that is all
  refusals has been checked and found *unverifiable*, which is information — but
  not endorsement.
- **A green receipt only proves the recorded command exited 0**, against the
  tree as it was at that moment. It is not proof the tests are meaningful, nor
  that they cover the change.

## How an agent could still evade it

These are the honest gaps. Some are inherent to a local, deterministic,
non-executing verifier; some are recall limits we are actively narrowing
(see [`docs/EXTRACTION_AUDIT.md`](EXTRACTION_AUDIT.md) and the benchmark table in
[`BENCHMARKS.md`](BENCHMARKS.md)).

1. **Don't make a checkable claim.** The single most effective evasion is to
   stay vague. "I improved the error handling" is Refused, not Contradicted —
   so an agent that reports only judgments is never caught lying because it
   never said anything falsifiable. `truth` shrinks this space (it scans the
   raw `message` as a backstop, and the more concrete the prose the more it
   catches) but cannot eliminate it. **A wall of refusals is itself a signal —
   and truth now surfaces it:** a turn with several claim-shaped segments where
   *nothing* was checkable is flagged (`all_refused` in the JSON, a ⚠ line in
   the text) so "I did a lot of vague work" doesn't read as a clean pass.

2. **Phrase a true-but-misleading claim.** Verdicts are per-claim. An agent can
   state four things that are each individually Supported and still leave you
   with a false overall impression by omitting the one that matters. `truth`
   checks the claims made; it cannot check the claim withheld.

3. **Exploit extraction recall.** The deterministic regex extractor does not
   parse every phrasing. A claim it can't parse becomes **Refused**, never a
   false Supported — so this degrades to "missed catch," not "wrong verdict."
   The `H*` band in the eval corpus is exactly these known misses. Supplying
   `claims` from the agent, or enabling the LLM/AST extractors, raises recall.

4. **Make a green receipt then change the code... carefully.** Receipt
   freshness is checked against your last working-tree edit time. The freshness
   rule closes the obvious "test, then edit, then claim" hole. An agent whose
   test doesn't *exercise* the change still gets a technically-true "tests pass"
   — `truth` can't know coverage without executing. But the two most common
   *empty* receipts are now caught deterministically from what was recorded: a
   run that reported **zero tests executed** ("0 tests run", "collected 0
   items") and a **scope-narrowed** command (`--test X`, `pytest path::case`,
   `-k expr`) are still Supported (they did exit 0) but flagged **weak** with
   lowered confidence and a caveat — a green subset run doesn't read as a green
   suite. What remains uncatchable: a real test that runs but is too shallow to
   fail on the bug.

5. **Stale index on existence claims.** The index auto-refreshes incrementally,
   and diff-based claims ("I just changed X") bypass the index entirely and
   read the working tree. But a pure *existence* claim resolved against an index
   that failed to refresh could be wrong. `truth` reports index emptiness/
   staleness in the `index` block and refuses to pass a "clean" result blindly;
   still, trust the diff over the index, which is why the engine ranks it that
   way.

6. **Tamper with the evidence itself.** Everything below the verifier is
   trusted (next section). An agent that can run arbitrary shell can, in
   principle, write `.truth/` directly, forge a receipt with `record-run`, or
   edit files between the check and your reading of it. The hooks make this
   harder (receipts are only recorded from real hook payloads with a real exit
   code, never guessed), but `truth` is a verifier, not a sandbox.

## Trust boundaries

`truth` trusts, and does **not** attempt to verify:

- **The working tree and git history** it reads. If these are wrong or
  manipulated, verdicts derived from them are wrong.
- **The `.truth/` SQLite store.** Receipts and the audit trail are only as
  trustworthy as write-access to that file. Keep it where only you (and your
  agent under your control) can write.
- **The `truth` / `truth-mcp` binaries.** Verify the download with the
  published checksum (`install.sh` does this automatically; see the README
  "Install" section). A replaced binary can say anything.
- **The host.** A compromised machine can defeat any local tool. `truth`'s
  guarantee is "I read your real code and your real diff and applied fixed
  rules" — it is not a guarantee about a machine you don't control.

## Why local, and what that buys you

Verdicts come from *your* working tree, *your* git diff, and *your* recorded
runs — none of which a remote service can see. Running locally is what makes the
evidence real; it also means your code never leaves the machine. The cost is the
trust boundary above: the tool is exactly as trustworthy as the local
environment it runs in. For a fact-checker, that is the right trade — the
alternative is trusting a model's say-so, which is the problem `truth` was built
to replace.

## One-line summary

> `truth` can prove that a **concrete, checkable** claim **disagrees** with your
> real code, diff, or recorded runs. It cannot prove a claim is *fully* true,
> that the code is *correct*, or catch an agent that simply never says anything
> falsifiable. A Contradicted verdict is a caught lie; a Supported verdict is
> "consistent with the evidence I could read"; a Refused verdict is honest
> ignorance — and the three are not interchangeable.
