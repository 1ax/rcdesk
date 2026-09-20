# Build context is the repository root (this is a Cargo workspace):
#   docker build -f server/Dockerfile -t rcdesk-server .

FROM rust:1-bookworm AS builder
WORKDIR /app

COPY Cargo.toml Cargo.lock ./
COPY proto/ proto/
COPY server/ server/

# `host` is a workspace member with heavy, partly platform-specific
# dependencies (webrtc, openh264, and -- behind `cfg(target_os = ...)` --
# scap/enigo/objc2/windows) that this image has no use for: `rcdesk-server`
# does not depend on `rcdesk-host`. Cargo still needs every workspace
# member's Cargo.toml and the source paths it points at to resolve the
# workspace, so stub the source instead of dragging the real crate (and its
# non-cfg-gated deps: webrtc, rtc, openh264, clap, tokio-tungstenite, ...)
# into this build. `cargo build -p rcdesk-server` below only compiles
# rcdesk-server's own dependency graph, so the stub is never actually built.
COPY host/Cargo.toml host/Cargo.toml
RUN mkdir -p host/src && \
    printf 'fn main() {}\n' > host/src/main.rs && \
    touch host/src/lib.rs

RUN cargo build --release -p rcdesk-server

FROM debian:bookworm-slim
RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates curl && \
    rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/rcdesk-server /usr/local/bin/rcdesk-server

ENV RCDESK_PORT=8080
EXPOSE 8080

# Docker inherits the owner/permissions from the IMAGE's directory when it
# first initializes an EMPTY named volume mounted at that path -- so
# creating /data as nobody:nogroup here means nobody can write into the
# volume on the VPS without any manual `chown` (the `rcdesk` deploy user
# there has no root to run one).
RUN mkdir -p /data && chown nobody:nogroup /data
ENV RCDESK_DB_PATH=/data/rcdesk.db

USER nobody

HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD curl -fsS http://127.0.0.1:8080/healthz || exit 1

CMD ["rcdesk-server"]
