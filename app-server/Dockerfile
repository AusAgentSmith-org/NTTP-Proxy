# Builds inside Docker — only depends on public crates.

FROM rust:1.90-slim AS builder
RUN apt-get update && apt-get install -y pkg-config libssl-dev && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY Cargo.toml Cargo.lock* ./
COPY src/ src/
RUN cargo build --release

FROM debian:trixie-slim
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/nzbservice-app-server /usr/local/bin/nzbservice-app-server
COPY static /app/static

ENV LOG_DIR=/logs
ENV STATIC_DIR=/app/static
ENV LISTEN_PORT=8090

EXPOSE 8090
VOLUME ["/logs"]

CMD ["nzbservice-app-server"]
