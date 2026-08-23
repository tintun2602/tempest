# ---- Build stage ----
FROM rust:1.85-bookworm AS builder
WORKDIR /app

# Cache dependencies: copy manifests, build with a dummy main
COPY Cargo.toml Cargo.lock* ./
RUN mkdir src && echo "fn main() {}" > src/main.rs
RUN cargo build --release
RUN rm -rf src

# Build the real binary
COPY src ./src
RUN touch src/main.rs && cargo build --release

# ---- Runtime stage ----
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/tempest /usr/local/bin/tempest
CMD ["tempest"]
