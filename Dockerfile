# Multi-stage build for optimized production image
# Bases are pinned by digest so an identical source revision always produces an
# identical image. Refresh with:
#   docker manifest inspect rust:1.98-slim-bookworm -v | jq -r '.[0].Descriptor.digest'
FROM rust:1.98-slim-bookworm@sha256:af0579d28b9a7ec5251aaafcb0c0a23dcde5c97065112aae0cc3abeda42d5394 AS builder

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
FROM debian:bookworm-slim@sha256:5ae3c39ebd15e229dcedd5cee596b2497182493d41ff162e824ba13fc1b2b867

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
