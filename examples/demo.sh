#!/bin/bash
# Scripted recording of truth's README demo. EVERY command and every byte of
# output shown is real — the only theatrics are the dimmed narrator comments
# and simulated keystroke pacing. Record with:
#   asciinema rec --cols 104 --rows 34 -c "bash examples/demo.sh" demo.cast
#   agg --theme dracula --font-size 16 demo.cast demo.gif
set -eu

TRUTH="${TRUTH_BIN:-truth}"
TRUTH_MCP="${TRUTH_MCP_BIN:-truth-mcp}"

# ---- stage the repo (off-camera) --------------------------------------------
# The ground truth the agent will lie about: MAX_RETRIES is 3, parse_legacy
# still exists, the only real change this turn is the /v1/refund route, and
# the last test run failed.
T="/tmp/acme"
rm -rf "$T"
mkdir -p "$T/src"
cd "$T"
git init -q .
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
cat >> src/api.rs <<'EOF'

pub fn refund_routes(app: &mut Router) {
    app.get("/v1/refund", refund);
}
EOF
"$TRUTH" record-run --command "cargo test" --exit-code 101 >/dev/null 2>&1

# The MCP request the agent's harness sends (newline-delimited JSON-RPC).
cat > verify_turn.json <<'EOF'
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"verify_turn","arguments":{"message":"Done! I added the /v1/refund endpoint, set MAX_RETRIES to 5, renamed parse_legacy to parse_v2, and tests pass.","claims":["I added the /v1/refund endpoint","I set MAX_RETRIES to 5","renamed parse_legacy to parse_v2","tests pass"],"repo":"/tmp/acme"}}}
EOF

# ---- the scene ---------------------------------------------------------------
type_cmd() {
    printf '\033[1;32m$\033[0m '
    for ((i = 0; i < ${#1}; i++)); do
        printf '%s' "${1:i:1}"
        sleep 0.016
    done
    printf '\n'
    sleep 0.4
}
dim() {
    printf '\033[2m%s\033[0m\n' "$1"
    sleep 0.8
}

clear
sleep 0.8
dim '# An AI coding agent just finished a turn and reported:'
sleep 0.2
printf '\033[33m'
cat <<'EOF'

  "Done! I added the /v1/refund endpoint, set MAX_RETRIES to 5,
   renamed parse_legacy to parse_v2 — and tests pass."

EOF
printf '\033[0m'
sleep 2.4

dim '# Before saying that, the agent calls the verify_turn MCP tool on itself.'
dim '# This is the exact request its harness sends to truth-mcp:'
echo
type_cmd "jq .params.arguments verify_turn.json"
jq .params.arguments verify_turn.json
sleep 3.2
echo
dim '# ...and the exact response it gets back:'
echo
type_cmd "truth-mcp < verify_turn.json | jq -r '.result.content[0].text'"
"$TRUTH_MCP" <verify_turn.json | jq -r '.result.content[0].text'
sleep 4.5
echo
dim '# Deterministic: verdicts come from the code, the git diff, and recorded'
dim '# test runs — never from a model. Listing a claim cannot make it pass.'
sleep 1.6
echo
dim "# Don't trust the agent to call it? Make it a gate:"
type_cmd "truth hook install"
"$TRUTH" hook install
sleep 2.4
echo
dim '# /plugin marketplace add blasrodri/truth  ·  github.com/blasrodri/truth'
sleep 2.0
