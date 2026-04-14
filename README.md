# nzbservice — Usenet proxy POC

Three components:

- **`nntp-proxy`** (this dir root) — Rust binary that accepts plain NNTP on :119 and brokers all client traffic through an authenticated TLS connection pool to a single upstream Usenet server. Credential substitution + pass-through of all NNTP commands including multi-line article/xover responses.
- **`gui/`** — web UI (Axum + plain HTML/JS) at http://localhost:8080: upload `.nzb` files, watch the queue, search stub. Downloads route through the proxy.
- **`client/`** — headless CLI client (ported from `NZBFailTest`). Useful for automated/repeatable testing; scale with `--scale client=N`.

## Layout

```
nzbservice/
├── src/                 nntp-proxy source
├── Cargo.toml           nntp-proxy manifest
├── Dockerfile           nntp-proxy Dockerfile (builds from source)
├── docker-compose.yml   proxy + gui + scalable clients
├── nzbs/                drop .nzb files here (for the CLI client)
├── downloads/           shared — gui + client write completed downloads here
├── logs/                per-service rolling logs
├── gui/                 web UI
│   ├── src/             Axum backend (upload, queue, stub search)
│   ├── static/          index.html + app.js + styles.css
│   ├── Cargo.toml
│   └── Dockerfile       packages pre-built binary + static assets
└── client/              headless CLI client
    ├── src/
    ├── Cargo.toml
    ├── Cargo.lock
    └── Dockerfile       packages a pre-built binary
```

## First-time setup

```bash
# Proxy builds fully inside Docker (only uses public crates).
# Client and GUI depend on the private Forgejo registry — pre-build on host:
(cd client && cargo build --release)
(cd gui && cargo build --release)

mkdir -p nzbs downloads logs
cp /path/to/your/*.nzb nzbs/
```

## Run

```bash
docker compose build
docker compose up                  # proxy + gui + 1 CLI client
docker compose up --scale client=3 # add more CLI clients, all sharing the 15-slot pool
```

Then open http://localhost:8080 for the GUI.

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
