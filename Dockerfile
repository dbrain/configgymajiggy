# Multi-stage build for optimized production image
FROM rust:1.98-slim-bookworm AS builder

WORKDIR /usr/src/app

# Copy manifest files
COPY Cargo.toml Cargo.lock ./

# Create dummy sources to cache the dependency build
RUN mkdir src && \
    echo "fn main() {}" > src/main.rs && \
    echo "" > src/lib.rs && \
    cargo build --release --locked

# Copy real source code
COPY src ./src

# COPY preserves context mtimes, which can be older than the dummy build's
# fingerprint - touching every source forces cargo to actually rebuild.
RUN find src -type f -exec touch {} + && cargo build --release --locked

# Production stage
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

RUN groupadd -r appuser && useradd -r -g appuser appuser

WORKDIR /app

# root-owned and read-only: the runtime user must not be able to rewrite its
# own executable.
COPY --from=builder --chown=root:root --chmod=0555 \
    /usr/src/app/target/release/configgymajiggy /app/configgymajiggy

USER appuser

EXPOSE 8080

# The binary probes itself, so the image needs no HTTP client of its own.
HEALTHCHECK --interval=30s --timeout=10s --start-period=5s --retries=3 \
    CMD ["/app/configgymajiggy", "--health-check"]

CMD ["/app/configgymajiggy"]
