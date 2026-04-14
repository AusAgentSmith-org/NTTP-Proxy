# nzbservice — Usenet proxy POC

Two components:

- **`nntp-proxy`** (this dir root) — Rust binary that accepts plain NNTP on :119 and brokers all client traffic through an authenticated TLS connection pool to a single upstream Usenet server. Credential substitution + pass-through of all NNTP commands including multi-line article/xover responses.
- **`client/`** — copy of `NZBFailTest`, reused as the test client. Env-var-configurable so multiple containers can each point at the proxy and download NZBs.

## Layout

```
nzbservice/
├── src/                 nntp-proxy source
├── Cargo.toml           nntp-proxy manifest
├── Dockerfile           nntp-proxy Dockerfile (builds from source)
├── docker-compose.yml   proxy + scalable clients
├── nzbs/                drop .nzb files here
├── downloads/           clients write completed downloads here
└── client/              test client
    ├── src/
    ├── Cargo.toml
    ├── Cargo.lock
    └── Dockerfile       packages a pre-built binary
```

## First-time setup

```bash
# Proxy builds fully inside Docker (only uses public crates).
# Client must be pre-built on the host since its deps live on the private
# Forgejo registry (unreachable inside Docker):
(cd client && cargo build --release)

mkdir -p nzbs downloads
cp /path/to/your/*.nzb nzbs/
```

## Run

```bash
docker compose build
docker compose up                  # 1 client
docker compose up --scale client=3 # 3 clients sharing the 15-slot proxy pool
```

## Config

Proxy (set in `docker-compose.yml` or `.env`):

| Env | Default |
|---|---|
| `NNTP_HOST` | `aunews.frugalusenet.com` |
| `NNTP_PORT` | `563` |
| `NNTP_USER` | `sprooty` |
| `NNTP_PASS` | `3MemP7tRt` |
| `NNTP_CONNECTIONS` | `15` — cap on upstream connections |
| `LISTEN_PORT` | `119` |

Client:

| Env | Default |
|---|---|
| `NNTP_HOST` | `nntp-proxy` (service name) |
| `NNTP_PORT` | `119` |
| `NNTP_SSL` | `false` |
| `NNTP_CONNECTIONS` | `8` — per-client connections to the proxy |
| `NZB_DIR` | `/nzbs` |
| `BASE_DIR` | `/downloads` |
| `NZB_FILTER` | `all` — substring to match, or `all` |

## Rebuild cycle

```bash
# After changing nntp-proxy source:
docker compose build nntp-proxy

# After changing client source:
(cd client && cargo build --release) && docker compose build client
```
