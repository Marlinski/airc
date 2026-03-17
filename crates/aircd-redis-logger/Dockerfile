# Build stage
FROM rust:1.87-slim AS builder

WORKDIR /build

# Copy workspace manifests and all crate sources
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/

# Build only the aircd-redis-logger binary in release mode
RUN cargo build --release --bin aircd-redis-logger

# Runtime stage — minimal image
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/aircd-redis-logger /usr/local/bin/aircd-redis-logger

# Default log output directory — mount a volume here in production
RUN mkdir -p /var/log/airc
ENV AIRCD_LOG_DIR=/var/log/airc

# Default Redis URL — override via env var or docker-compose
ENV AIRCD_REDIS_URL=redis://redis:6379

# node_id stamped on log rows produced by this logger instance
ENV AIRCD_LOGGER_NODE_ID=logger

ENV RUST_LOG=info

ENTRYPOINT ["/usr/local/bin/aircd-redis-logger"]
