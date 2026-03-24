FROM rust:1-bookworm AS builder

WORKDIR /app

# Dependency caching: copy workspace and member manifests, then build a dummy binary
COPY Cargo.toml Cargo.lock ./
COPY server/Cargo.toml server/Cargo.toml
COPY hook/Cargo.toml hook/Cargo.toml
RUN mkdir -p server/src hook/src \
    && echo 'fn main() {}' > server/src/main.rs \
    && echo 'fn main() {}' > hook/src/main.rs \
    && cargo build --release -p claude-notify \
    && rm -rf server/src hook/src target/release/deps/claude_notify*

# Build real source (server only)
COPY server/src/ server/src/
COPY server/templates/ server/templates/
RUN cargo build --release -p claude-notify

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/claude-notify /usr/local/bin/claude-notify
ENTRYPOINT ["claude-notify", "serve"]
