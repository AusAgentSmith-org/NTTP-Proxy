# Pre-build the binary on the host first:
#   (cd gui && cargo build --release)
#
# Same pattern as client: host binary + static assets packaged into a trixie-slim runtime.

FROM debian:trixie-slim
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*

COPY target/release/nzbservice-gui /usr/local/bin/nzbservice-gui
COPY static /app/static

ENV BASE_DIR=/downloads
ENV LOG_DIR=/logs
ENV STATIC_DIR=/app/static
ENV LISTEN_PORT=8080

EXPOSE 8080
VOLUME ["/downloads", "/logs"]

ENTRYPOINT ["nzbservice-gui"]
