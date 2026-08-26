# Multi-stage build for optimized production image
FROM rust:1.98-slim-bookworm AS builder

WORKDIR /usr/src/app

# Copy manifest files
COPY Cargo.toml Cargo.lock ./

# Create dummy source to cache dependencies
RUN mkdir src && \
    echo "fn main() {}" > src/main.rs && \
    cargo build --release --locked

# Copy real source code
COPY src ./src

# COPY preserves context mtimes, which can be older than the dummy build's
# fingerprint - touch forces cargo to actually rebuild the binary.
RUN touch src/main.rs && cargo build --release --locked

# Production stage
FROM debian:bookworm-slim

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    curl \
    && rm -rf /var/lib/apt/lists/*

# Create app user for security
RUN groupadd -r appuser && useradd -r -g appuser appuser

# Create app directory
WORKDIR /app

# Copy binary from builder stage
COPY --from=builder /usr/src/app/target/release/configgymajiggy /app/configgymajiggy

# Change ownership to app user
RUN chown -R appuser:appuser /app

# Switch to non-root user
USER appuser

# Expose port
EXPOSE 8080

# Health check
HEALTHCHECK --interval=30s --timeout=10s --start-period=5s --retries=3 \
    CMD curl -f http://localhost:8080/health || exit 1

# Run the application
CMD ["./configgymajiggy"]
