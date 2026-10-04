# syntax=docker/dockerfile:1

# Builder and runtime share the same Debian release so the binary's glibc matches.
FROM rust:1-slim-trixie AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
# Build dependencies in their own layer so source edits don't rebuild them.
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
 && cargo build --release --locked \
 && rm -rf src
COPY src ./src
RUN touch src/main.rs && cargo build --release --locked

# Claude Code, native installer, pinned. Upgrade by rebuilding with a new CLAUDE_CODE_VERSION.
FROM debian:trixie-slim AS claude
ARG CLAUDE_CODE_VERSION=2.1.289
RUN apt-get update \
 && apt-get install -y --no-install-recommends bash ca-certificates curl \
 && rm -rf /var/lib/apt/lists/*
RUN curl -fsSL https://claude.ai/install.sh | bash -s "${CLAUDE_CODE_VERSION}" \
 && cp -L /root/.local/bin/claude /usr/local/bin/claude \
 && /usr/local/bin/claude --version

FROM debian:trixie-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates tini \
 && rm -rf /var/lib/apt/lists/*
COPY --from=claude /usr/local/bin/claude /usr/local/bin/claude
COPY --from=builder /app/target/release/claude-session-starter /usr/local/bin/claude-session-starter
RUN useradd -m -u 1000 app && mkdir /data && chown app /data
USER app
# No CLAUDE.md, settings or project files anywhere: the starter runs in the empty /data/work.
ENV DATA_DIR=/data \
    DISABLE_AUTOUPDATER=1
VOLUME /data
# tini is PID 1: it forwards SIGTERM to the service and reaps orphaned `claude` helpers.
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/claude-session-starter"]
CMD ["daemon"]
