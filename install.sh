#!/bin/sh
# Install the latest truth release (truth + truth-mcp) for this platform.
#
#   curl -fsSL https://raw.githubusercontent.com/blasrodri/truth/main/install.sh | sh
#
# Downloads the prebuilt tarball from the latest GitHub release and installs
# both binaries to /usr/local/bin (or ~/.local/bin when that isn't writable).
# No Rust toolchain needed.
set -eu

REPO="blasrodri/truth"

case "$(uname -s)" in
  Darwin) os="apple-darwin" ;;
  Linux) os="unknown-linux-gnu" ;;
  *) echo "unsupported OS: $(uname -s) (build from source: cargo build --release --workspace)" >&2; exit 1 ;;
esac
case "$(uname -m)" in
  arm64 | aarch64) arch="aarch64" ;;
  x86_64 | amd64) arch="x86_64" ;;
  *) echo "unsupported arch: $(uname -m) (build from source: cargo build --release --workspace)" >&2; exit 1 ;;
esac
target="${arch}-${os}"

# Fetch fully before grepping — piping into `grep -m1` SIGPIPEs curl and
# prints a spurious "Failed writing body".
release_json=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest")
tag=$(printf '%s' "$release_json" | grep -m1 '"tag_name"' | cut -d'"' -f4)
[ -n "$tag" ] || { echo "could not determine the latest release" >&2; exit 1; }

url="https://github.com/$REPO/releases/download/$tag/truth-$tag-$target.tar.gz"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

echo "Downloading truth $tag for $target ..."
curl -fsSL "$url" | tar -xz -C "$tmp"

dest="/usr/local/bin"
if [ ! -w "$dest" ]; then
  dest="$HOME/.local/bin"
  mkdir -p "$dest"
fi
# Remove first: on macOS, copying over a running/signed binary in place can
# invalidate its code signature (the old inode keeps the stale signature).
rm -f "$dest/truth" "$dest/truth-mcp"
install "$tmp"/truth-*/truth "$tmp"/truth-*/truth-mcp "$dest/"

echo "Installed truth and truth-mcp $tag to $dest"
case ":$PATH:" in
  *":$dest:"*) ;;
  *) echo "NOTE: $dest is not on your PATH — add it to use \`truth\` directly." ;;
esac
"$dest/truth" --version

cat <<'NEXT'

Next steps (per repo):
  truth init            # one-time: create the .truth store
  truth hook install    # Claude Code: fact-check every agent turn
  claude mcp add --scope user truth -- truth-mcp   # or register the MCP tool
NEXT
