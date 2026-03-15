# ============================================================================
# aircd — multi-stage Docker build
# ============================================================================
# Build:  docker build -t aircd .
# Run:    docker run -d --name aircd \
#           -p 6667:6667 -p 6697:6697 -p 8080:8080 \
#           -v aircd-logs:/var/log/aircd \
#           -e AIRCD_NAME=irc.openlore.xyz \
#           aircd

# ---------------------------------------------------------------------------
# Stage 1: build
# ---------------------------------------------------------------------------
FROM rust:1.85-bookworm AS builder

# Install protoc (required by prost-build).
RUN apt-get update && apt-get install -y protobuf-compiler && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY . .

RUN cargo build --release --package aircd

# ---------------------------------------------------------------------------
# Stage 2: runtime
# ---------------------------------------------------------------------------
FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /src/target/release/aircd /usr/local/bin/aircd

# Log volume — mount to persist channel logs across restarts.
VOLUME /var/log/aircd

# Default environment — all overridable at runtime.
ENV AIRCD_BIND=0.0.0.0:6667 \
    AIRCD_NAME=airc.local \
    AIRCD_HTTP_PORT=8080 \
    AIRCD_LOG_DIR=/var/log/aircd \
    RUST_LOG=info

# IRC (plaintext), IRC (TLS), HTTP/WS API
EXPOSE 6667 6697 8080

# Run in foreground (no daemonization inside a container).
ENTRYPOINT ["aircd", "start", "--foreground"]
