# dist/truth-mcp — prebuilt static binary for Glama

`truth-mcp` here is a fully-static `x86_64-unknown-linux-musl` build of the MCP
server. It is checked in so Glama's deployment can run the server **without
compiling Rust** — Glama clones the repo to `/app` and starts the binary
directly, which keeps the container startup inside Glama's health-check ping
window.

Glama build spec (Dockerfile admin page):

- Base image: `debian:trixie-slim` (the static binary needs no runtime deps)
- Build steps: *(none — nothing to compile)*
- CMD arguments: `["/app/dist/truth-mcp"]`

## Rebuilding after a code change

The binary is reproducible via the musl cross toolchain:

```sh
docker run --rm -v "$PWD":/src -w /src messense/rust-musl-cross:x86_64-musl \
  cargo build --release --locked --target x86_64-unknown-linux-musl -p truth-mcp
cp target/x86_64-unknown-linux-musl/release/truth-mcp dist/truth-mcp
```

Keep this in sync with the workspace version in `Cargo.toml` when cutting a
Glama release.
