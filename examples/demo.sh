#!/bin/bash
# Scripted demo of truth catching an agent's lies. Used to record the README
# cast:  asciinema rec -c ./examples/demo.sh demo.cast && agg demo.cast demo.gif
# Reproducible: stages a scratch repo off-camera, then plays the scene.
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

# The agent's actual change this turn: it ONLY added the /v1/refund route.
cat >> src/api.rs <<'EOF'

pub fn refund_routes(app: &mut Router) {
    app.get("/v1/refund", refund);
}
EOF

# The agent ran the tests... and they failed.
"$TRUTH" record-run --command "cargo test" --exit-code 101 >/dev/null 2>&1

# ---- the scene --------------------------------------------------------------
type_cmd() {
    printf '\033[1;32m$\033[0m '
    for ((i = 0; i < ${#1}; i++)); do
        printf '%s' "${1:i:1}"
        sleep 0.018
    done
    printf '\n'
    sleep 0.4
}
say() {
    printf '\033[2m%s\033[0m\n' "$1"
    sleep 0.9
}

clear
sleep 0.6
say '# Your AI coding agent just reported:'
sleep 0.2
printf '\033[33m'
cat <<'EOF'

  "Done! I added the /v1/refund endpoint, set MAX_RETRIES to 5,
   renamed parse_legacy to parse_v2 — and tests pass."

EOF
printf '\033[0m'
sleep 2.2

say '# One of those claims is true. truth checks all of them:'
sleep 0.4
type_cmd "truth verify-turn \"I added /v1/refund, I set MAX_RETRIES to 5, renamed parse_legacy to parse_v2, and tests pass\""
"$TRUTH" verify-turn "I added /v1/refund, I set MAX_RETRIES to 5, renamed parse_legacy to parse_v2, and tests pass" || true
sleep 3.0

echo
say '# Every verdict is decided from evidence — the code, the git diff,'
say '# recorded test runs — never by a model. The agent cannot argue with it.'
sleep 1.4
echo
say '# Make it automatic: fact-check every agent turn, block the lies:'
type_cmd "truth hook install"
"$TRUTH" hook install
sleep 2.6
echo
say '# github.com/blasrodri/truth'
sleep 1.6
