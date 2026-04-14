# Pre-build the binary on the host first:
#   cd ~/Working/apps/Arz/NZBFailTest && cargo build --release
#
# Then docker compose build will package the pre-built binary.
# The build context is the NZBFailTest directory (set in docker-compose.yml).

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*

COPY target/release/nzb-fail-test /usr/local/bin/nzb-fail-test

ENV BASE_DIR=/downloads
ENV NZB_DIR=/nzbs
ENV NZB_FILTER=all

VOLUME ["/nzbs", "/downloads"]

ENTRYPOINT ["nzb-fail-test"]
