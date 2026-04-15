# nzbservice — Usenet platform POC

Four cooperating services, all under one `docker compose`:

| Service | Port | What it does |
|---|---|---|
| **`app-server`** | 8090 | User accounts, per-user connection caps, lock control, activity tracking. Admin UI included. |
| **`nntp-proxy`** | 1119 | Plain NNTP in, TLS out to one upstream Usenet server. Validates every `AUTHINFO` against the app-server, enforces per-user caps, drops sessions on lock. |
| **`gui`** | 8080 | Web UI: `.nzb` upload, queue (with per-job cancel), `nzb.indexarr` search + grab. |
| **`client`** | — | Headless CLI client (CLI-only consumer of the proxy). Useful for repeatable load tests; scale with `--scale client=N`. |

```
                    ┌──────────────┐
                    │  app-server  │ :8090  ─── Admin UI ───
                    └──────┬───────┘
                  validate │ activity │ locked
                           ▼
   user → http :8080 ───►  gui ──┐
                                  ├─ NNTP :119 ──► nntp-proxy ──► Usenet provider (TLS)
                          client ─┘
```

## Layout

```
nzbservice/
├── README.md  CLAUDE.md  docs/
├── src/                 nntp-proxy source
├── Cargo.toml           nntp-proxy manifest
├── Dockerfile           proxy: builds from source inside Docker
├── docker-compose.yml   all four services
├── .env                 INDEXARR_API_KEY, ADMIN_TOKEN, PROXY_TOKEN, BOOTSTRAP_*
├── nzbs/                drop .nzb files here for the CLI client
├── downloads/           gui + client write completed downloads here
├── logs/                per-service rolling daily logs
├── app-server/          user + lock + activity service
│   ├── src/             Axum backend
│   ├── static/          admin UI (HTML/JS/CSS)
│   ├── Cargo.toml
│   └── Dockerfile       builds inside Docker (public crates only)
├── gui/                 web client
│   ├── src/             Axum backend (upload, queue, search, grab, cancel)
│   ├── static/          index.html + app.js + styles.css
│   ├── Cargo.toml
│   └── Dockerfile       packages pre-built host binary + static assets
└── client/              CLI test client (NZBFailTest port)
    ├── src/
    ├── Cargo.toml
    └── Dockerfile       packages a pre-built host binary
```

## First-time setup

`gui` and `client` depend on the private Forgejo cargo registry, so they must be **pre-built on the host** (where Forgejo is reachable). `nntp-proxy` and `app-server` use only public crates and build entirely inside Docker.

```bash
cd ~/Working/apps/nzbservice
(cd gui    && cargo build --release)
(cd client && cargo build --release)
mkdir -p nzbs downloads logs
docker compose build
docker compose up -d
```

Then:

- http://localhost:8080 — **GUI**: upload, queue, search, grab.
- http://localhost:8090 — **Admin UI**: paste `ADMIN_TOKEN` (default `admin-dev-token`), manage users.

A bootstrap user (`guiuser` / `guipass`, max 8 connections) is created automatically on first start so the gui + client work out of the box.

## Config (`.env`)

```env
# nzb.indexarr search
INDEXARR_URL=https://nzb.indexarr.net
INDEXARR_API_KEY=...

# App-server tokens — change for any non-toy deployment
ADMIN_TOKEN=admin-dev-token       # admin UI bearer
PROXY_TOKEN=proxy-dev-token       # proxy ↔ app-server bearer

# Bootstrap user, created on first start of app-server
BOOTSTRAP_USER=guiuser
BOOTSTRAP_PASS=guipass
BOOTSTRAP_MAX_CONNECTIONS=8
```

Other key knobs (in `docker-compose.yml`):

| Service | Env | Default | Notes |
|---|---|---|---|
| nntp-proxy | `NNTP_HOST/PORT/USER/PASS` | frugalusenet | upstream Usenet server |
| nntp-proxy | `NNTP_CONNECTIONS` | `15` | provider account connection cap |
| nntp-proxy | `APP_SERVER_URL` | `http://app-server:8090` | empty disables (open mode) |
| nntp-proxy | `REPORT_INTERVAL_SECS` | `5` | activity report cadence |
| nntp-proxy | `LOCK_POLL_INTERVAL_SECS` | `2` | how often locks are polled |
| gui | `MAX_ACTIVE_DOWNLOADS` | `2` | concurrent jobs per gui |

## Rebuild cycle

```bash
# Proxy or app-server (Rust code in src/, builds in Docker):
docker compose build nntp-proxy && docker compose up -d nntp-proxy
docker compose build app-server && docker compose up -d app-server

# Gui (Rust + static):
(cd gui && cargo build --release) && docker compose build gui && docker compose up -d gui

# Client (CLI):
(cd client && cargo build --release) && docker compose build client
docker compose up -d --scale client=2
```

## Logs

Each service writes daily-rolled logs to `./logs/`:

```bash
tail -f logs/app-server.log.*
tail -f logs/nntp-proxy.log.*
tail -f logs/gui.log.*
tail -f logs/nzbservice-client-*.log.*
```

`docker logs` only shows the one-line startup message per container.
