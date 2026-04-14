# ── Stage 1: build ────────────────────────────────────────────────────────────
FROM rust:1.82-slim AS builder

# System deps for ring / aws-lc-rs
RUN apt-get update && apt-get install -y cmake pkg-config libssl-dev && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY Cargo.toml Cargo.lock* ./
COPY src/ src/

RUN cargo build --release

# ── Stage 2: runtime ──────────────────────────────────────────────────────────
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/nntp-proxy /usr/local/bin/nntp-proxy

EXPOSE 119

CMD ["nntp-proxy"]
