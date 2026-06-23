#!/usr/bin/env bash
# Precision gate: assert truth's false-contradiction rate is 0 on the labeled
# corpus. This is the adoption metric — a verifier that false-accuses a truthful
# agent is worse than no verifier. Recall (catch rate on real lies) is reported
# but never blocks.
#
# Narrative cases reference the token __EMPTY_REPO__; we stage the committed
# fixtures/precision/empty_repo to a GIT-LESS temp dir and substitute it, so
# diff-native claims get an empty diff (→ inconclusive) instead of resolving
# against the verifier's own working tree.
#
# Usage: scripts/precision_gate.sh [path/to/truth-binary]
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TRUTH="${1:-$ROOT/target/release/truth}"
CORPUS="$ROOT/fixtures/precision"

# Stage the empty fixture repo OUTSIDE any git repo so `git -C` finds no repo
# (→ no diff → inconclusive). Copying strips it from the parent repo's tree.
STAGE="$(mktemp -d "${TMPDIR:-/tmp}/truth_precision.XXXXXX")"
trap 'rm -rf "$STAGE"' EXIT
cp -R "$CORPUS/empty_repo" "$STAGE/empty_repo"
rm -rf "$STAGE/empty_repo/.git" 2>/dev/null || true

# Substitute the token in each narrative case file into a temp fixture.
gen_fixture() {
  local src="$1" dst="$2"
  sed "s#__EMPTY_REPO__#$STAGE/empty_repo#g" "$src" > "$dst"
}

fail=0

echo "=== PRECISION GATE (false_contradictions == 0, hard) ==="
for bucket in narrative_prose true_claims; do
  f="$CORPUS/$bucket.yaml"
  [ -f "$f" ] || continue
  tmp="$STAGE/$bucket.yaml"
  gen_fixture "$f" "$tmp"
  if ! "$TRUTH" eval "$tmp" --precision; then
    fail=1
  fi
  echo
done

echo "=== RECALL (advisory, non-blocking) ==="
for bucket in real_lies; do
  f="$CORPUS/$bucket.yaml"
  [ -f "$f" ] || continue
  tmp="$STAGE/$bucket.yaml"
  gen_fixture "$f" "$tmp"
  # Only run if cases have real repos (not the TODO stubs).
  if grep -q "TODO repo" "$f"; then
    echo "  $bucket: repos not yet vendored — skipping recall (see TODO markers)"
  else
    "$TRUTH" eval "$tmp" --precision || true
  fi
  echo
done

if [ "$fail" -ne 0 ]; then
  echo "PRECISION GATE FAILED — truth false-contradicted a truthful claim."
  exit 1
fi
echo "PRECISION GATE PASSED — 0 false contradictions."
