# Pre-build the binary on the host first:
#   (cd client && cargo build --release)
#
# Uses debian:trixie-slim (glibc 2.41) to match newer host glibc versions.
# bookworm-slim only ships glibc 2.36 which is too old for binaries built
# on modern Ubuntu/Debian hosts (requires GLIBC_2.38+).

FROM debian:trixie-slim
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*

COPY target/release/nzb-fail-test /usr/local/bin/nzb-fail-test

ENV BASE_DIR=/downloads
ENV NZB_DIR=/nzbs
ENV NZB_FILTER=all

VOLUME ["/nzbs", "/downloads"]

ENTRYPOINT ["nzb-fail-test"]
