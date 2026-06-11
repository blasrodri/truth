#!/bin/bash
# Scripted demo of truth's Stop hook blocking a lying agent turn, used to
# record the README cast:
#   asciinema rec --cols 104 --rows 32 -c "bash examples/demo.sh" demo.cast
#   agg --theme dracula --font-size 16 demo.cast demo.gif
#
# The Claude Code session chrome is simulated for reproducibility; the
# verification is REAL: a scratch repo is staged with exactly the state the
# agent describes wrongly, and the block shown is the live output of
# `truth hook claude` on that repo — the same hook Claude Code runs.
set -eu

TRUTH="${TRUTH_BIN:-truth}"

# ---- stage the scene (silent, off-camera) ----------------------------------
T="/tmp/acme"
rm -rf "$T"
mkdir -p "$T"
cd "$T"
git init -q .
mkdir -p src
cat > src/config.rs <<'EOF'
pub const MAX_RETRIES: u32 = 3;
pub const REQUEST_TIMEOUT: u32 = 30;
EOF
cat > src/api.rs <<'EOF'
pub fn parse_legacy(input: &str) -> &str {
    input
}

pub fn routes(app: &mut Router) {
    app.get("/v1/checkout", checkout);
}
EOF
git add -A
git -c user.email=demo@truth -c user.name=demo commit -qm "baseline" >/dev/null

# What the agent ACTUALLY did this turn: only added the /v1/refund route.
cat >> src/api.rs <<'EOF'

pub fn refund_routes(app: &mut Router) {
    app.get("/v1/refund", refund);
}
EOF

# The agent ran the tests — they failed. (In a real session the PostToolUse
# hook records this receipt automatically.)
"$TRUTH" record-run --command "cargo test" --exit-code 101 >/dev/null 2>&1

# The agent's final (lying) message, as it lands in the session transcript.
AGENT_MSG="Done! I added the /v1/refund endpoint, set MAX_RETRIES to 5, renamed parse_legacy to parse_v2, and tests pass."
cat > transcript.jsonl <<EOF
{"type":"assistant","message":{"content":[{"type":"text","text":"$AGENT_MSG"}]}}
EOF

# Run the REAL hook now and capture its block, so the scene shows live output.
BLOCK_REASON=$(printf '{"hook_event_name":"Stop","stop_hook_active":false,"transcript_path":"%s/transcript.jsonl","cwd":"%s"}' "$T" "$T" \
    | "$TRUTH" hook claude \
    | python3 -c "import sys,json; print(json.load(sys.stdin)['reason'])")
[ -n "$BLOCK_REASON" ] || { echo "demo error: hook did not block" >&2; exit 1; }
rm -f transcript.jsonl

# ---- the scene --------------------------------------------------------------
type_text() { # simulated keystrokes
    for ((i = 0; i < ${#1}; i++)); do
        printf '%s' "${1:i:1}"
        sleep 0.022
    done
    printf '\n'
}
dim() { printf '\033[2m%s\033[0m\n' "$1"; }

clear
sleep 0.8
dim '# Claude Code, with the truth plugin installed'
echo
printf '\033[1m> \033[0m'
type_text "bump MAX_RETRIES to 5, rename parse_legacy to parse_v2, add a /v1/refund route"
sleep 1.2
echo
printf '\033[32m●\033[0m Update(src/api.rs)\n'
sleep 0.7
printf '\033[32m●\033[0m Bash(cargo test)\n'
printf '  \033[2m⎿ 1 test failed (exit 101) — recorded as a receipt by the truth hook\033[0m\n'
sleep 1.6
echo
printf '\033[1m⏺\033[0m %s\n' "$AGENT_MSG"
sleep 2.6
echo
printf '\033[31m■ Stop hook (truth) blocked this turn:\033[0m\n'
sleep 0.5
# The real hook output, captured above — verdict table and all.
printf '%s\n' "$BLOCK_REASON" | sed 's/^/  /' | sed -n '3,20p'
sleep 4.5
echo
dim '# The agent sees the evidence and corrects itself before you read anything:'
sleep 0.8
echo
printf '\033[1m⏺\033[0m I added the /v1/refund endpoint. Correction: MAX_RETRIES is still 3 —\n'
printf '  my config edit never applied — parse_legacy was not renamed, and the\n'
printf '  test suite is failing (exit 101). Fixing those now.\n'
sleep 3.4
echo
dim '# Deterministic verdicts from code, diff, and recorded runs. No AI judging AI.'
dim '# /plugin marketplace add blasrodri/truth  ·  github.com/blasrodri/truth'
sleep 2.0
