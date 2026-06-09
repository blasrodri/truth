# Builds the `truth-mcp` MCP server so registries (e.g. Glama) can start it and
# run an introspection check. Multi-stage: compile in a full Rust image, ship a
# slim runtime with just the binary.
#
#   docker build -t truth-mcp .
#   docker run -i --rm truth-mcp      # speaks MCP over stdio
#
# `git` is present at runtime because truth shells out to `git diff` for
# working-tree claims; a repo is expected to be mounted/cwd'd at use time.

FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
# Build only the MCP server (and its workspace deps) in release mode.
RUN cargo build --release --locked -p truth-mcp

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends git ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/truth-mcp /usr/local/bin/truth-mcp
# stdio JSON-RPC server: no ports, reads stdin / writes stdout.
ENTRYPOINT ["truth-mcp"]
