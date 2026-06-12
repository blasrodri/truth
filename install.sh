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

archive="truth-$tag-$target.tar.gz"
url="https://github.com/$REPO/releases/download/$tag/$archive"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

echo "Downloading truth $tag for $target ..."
# Download to disk (not piped into tar) so the bytes can be checksum-verified
# before anything is extracted or run.
curl -fsSL "$url" -o "$tmp/$archive"

# Verify the SHA-256 against the checksum the release publishes alongside the
# tarball. This is the integrity gate: a corrupted download or a tampered
# mirror fails here instead of installing a bad binary. (truth is a trust
# tool; it should not ask you to pipe an unverified binary into your shell.)
echo "Verifying checksum ..."
if curl -fsSL "$url.sha256" -o "$tmp/$archive.sha256" 2>/dev/null && [ -s "$tmp/$archive.sha256" ]; then
  # The .sha256 file is `<hash>  <filename>`; take only the hash so a path
  # mismatch between CI and here can't break verification.
  expected=$(awk '{print $1}' "$tmp/$archive.sha256" | head -n1)
  if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum "$tmp/$archive" | awk '{print $1}')
  elif command -v shasum >/dev/null 2>&1; then
    actual=$(shasum -a 256 "$tmp/$archive" | awk '{print $1}')
  else
    echo "WARNING: no sha256sum/shasum found — cannot verify checksum." >&2
    actual=""
  fi
  if [ -n "$actual" ]; then
    if [ "$expected" = "$actual" ]; then
      echo "Checksum OK ($actual)"
    else
      echo "CHECKSUM MISMATCH for $archive" >&2
      echo "  expected: $expected" >&2
      echo "  actual:   $actual" >&2
      echo "Refusing to install. The download may be corrupted or tampered with." >&2
      exit 1
    fi
  fi
else
  echo "WARNING: no published checksum for $archive — skipping verification." >&2
fi

tar -xzf "$tmp/$archive" -C "$tmp"

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

# Register the MCP server with Claude Code automatically when its CLI is
# around (idempotent; silent skip otherwise).
if command -v claude >/dev/null 2>&1; then
  if claude mcp get truth >/dev/null 2>&1; then
    echo "MCP: \`truth\` already registered with Claude Code."
  elif claude mcp add --scope user truth -- "$dest/truth-mcp" >/dev/null 2>&1; then
    echo "MCP: registered \`truth\` with Claude Code (user scope)."
  fi
fi

cat <<'NEXT'

Enable the agent gate (pick one):
  /plugin marketplace add blasrodri/truth && /plugin install truth@truth   # inside Claude Code
  truth hook install --user     # or: hooks for all repos, no plugin
  truth setup                   # or: project-scoped, current repo only

Hooks are zero-setup after that: any git repo you work in is fact-checked
(opt out with `truth hook auto off`).
NEXT
