# nzbservice — Usenet platform POC

Four cooperating services, all under one `docker compose`:

| Service | Host ports | What it does |
|---|---|---|
| **`app-server`** | `:8090` | Users + per-user connection caps + locks + activity tracking + session keys. Persistent (SQLite). Admin UI included. Exposes `GET /api/fingerprint` for clients that pin the proxy's NNTPS cert. |
| **`nntp-proxy`** | `:1119` plain, `:5563` NNTPS | Accepts NNTP (plain + TLS) from clients, proxies to one upstream provider over TLS. Validates every `AUTHINFO PASS` against the app-server (session key or password), enforces per-user cap, drops sessions on lock within ~2s. Self-signed cert generated on first start. |
| **`gui`** | `:8080` | Web UI with login: `.nzb` upload, queue (with per-job cancel), `nzb.indexarr` search + grab. Password → app-server session key → used as NNTP auth. |
| **`client`** | — | Headless CLI client. Same download engine as the gui. Useful for repeatable load tests — scale with `--scale client=N`. |

```
                    ┌──────────────────┐
      browser ◄───► │    app-server    │ :8090
      admin UI      │  users · locks · │
                    │  sessions · FP   │
                    └─┬────────▲───────┘
                      │login    │ validate / activity / locked
                      │session  │
                      │key      │
       ┌──────────────┘         │
       │                        │
       ▼                        │
    ┌──────┐                   ┌┴─────────────┐       ┌──────────┐
    │ gui  │ :8080 ──NNTP─────►│  nntp-proxy  │──TLS─►│ provider │
    │      │                   │ :119  :563   │       │          │
    └──────┘                   └──────────────┘       └──────────┘
    ┌──────┐  ──NNTP──────────►
    │client│
    └──────┘
```

## Auth model

1. User logs into the **gui** with `username + password`.
2. Gui asks **app-server** for a **session key** (48-char random, 30-day TTL by default).
3. Gui hot-swaps its NNTP config to use `username + session_key` (the real password is never sent to the proxy).
4. Proxy forwards each `AUTHINFO PASS` to the app-server for validation. App-server recognises the session key, returns `{allowed, max_connections}`.
5. Locking a user in admin revokes all their session keys *and* drops any active NNTP sessions within ~2s.

## Layout

```
nzbservice/
├── README.md  CLAUDE.md  docs/{architecture,todo}.md
├── src/                 nntp-proxy — Rust
│   ├── main.rs          plain + NNTPS listeners, background tasks
│   ├── pool.rs          upstream TLS pool with idle reuse + TTL
│   ├── session.rs       NNTP session state machine (generic over transport)
│   ├── user_pool.rs     per-user semaphore + cancellable session registry
│   ├── app_client.rs    HTTP client for app-server
│   ├── tls.rs           self-signed cert generation / loading
│   ├── config.rs        env-driven config
│   └── *tests*          4 UserPool unit tests
├── Cargo.toml  Dockerfile   proxy: builds from source in Docker
├── docker-compose.yml       all four services
├── .env                     INDEXARR_API_KEY, ADMIN_TOKEN, PROXY_TOKEN, BOOTSTRAP_*
├── data/                    SQLite DB + TLS cert (gitignored)
├── nzbs/                    drop .nzb files here for the CLI client (gitignored)
├── downloads/               gui + client completed downloads (gitignored)
├── logs/                    per-service rolling daily logs (gitignored)
├── app-server/              user + lock + activity + sessions + FP
│   ├── src/
│   │   ├── main.rs          router wiring + background tasks
│   │   ├── handlers.rs      HTTP handlers (admin, proxy, auth, fingerprint)
│   │   ├── store.rs         SQLite-backed Store
│   │   ├── state.rs         AppState + Config
│   │   └── *tests*          5 unit + 4 integration
│   ├── static/              admin UI (HTML/JS/CSS)
│   ├── Cargo.toml  Dockerfile  builds inside Docker
├── gui/                     web client
│   ├── src/
│   │   ├── main.rs          upload/queue/search/grab/login/logout
│   │   └── app_client.rs    login/logout against app-server
│   ├── static/              index.html + app.js + styles.css
│   ├── Cargo.toml  Dockerfile  packages pre-built host binary
└── client/                  CLI test client (NZBFailTest port)
    ├── src/
    ├── Cargo.toml
    └── Dockerfile           packages pre-built host binary
```

## First-time setup

`gui` and `client` depend on the private Forgejo cargo registry, so they must be **pre-built on the host** (where Forgejo is reachable). `nntp-proxy` and `app-server` use only public crates and build entirely inside Docker.

```bash
cd ~/Working/apps/nzbservice
(cd gui    && cargo build --release)
(cd client && cargo build --release)
mkdir -p nzbs downloads logs data
docker compose build
docker compose up -d
```

Then:

- http://localhost:8080 — **GUI**: login (bootstrap creds `guiuser` / `guipass`), upload, queue, search, grab.
- http://localhost:8090 — **Admin UI**: paste `ADMIN_TOKEN` (default `admin-dev-token`), manage users + view live activity.
- `openssl s_client -connect localhost:5563` — NNTPS listener, pinned fingerprint at `/api/fingerprint`.

Bootstrap user is created on first app-server start *if the DB is fresh* (existing users are preserved).

## Config (`.env`)

```env
# nzb.indexarr search (already-deployed indexer we proxy queries to)
INDEXARR_URL=https://nzb.indexarr.net
INDEXARR_API_KEY=...

# App-server tokens — change for any non-toy deployment
ADMIN_TOKEN=admin-dev-token       # admin UI bearer
PROXY_TOKEN=proxy-dev-token       # proxy ↔ app-server bearer

# Bootstrap user, created on first start of app-server (if DB is fresh)
BOOTSTRAP_USER=guiuser
BOOTSTRAP_PASS=guipass
BOOTSTRAP_MAX_CONNECTIONS=8
```

Other key knobs (in `docker-compose.yml`):

| Service | Env | Default | Notes |
|---|---|---|---|
| nntp-proxy | `NNTP_HOST/PORT/USER/PASS` | frugalusenet | upstream provider |
| nntp-proxy | `NNTP_CONNECTIONS` | `15` | provider account cap |
| nntp-proxy | `TLS_PORT` | `563` | 0 disables NNTPS |
| nntp-proxy | `TLS_DIR` | `/data/tls` | cert.pem + key.pem live here |
| nntp-proxy | `APP_SERVER_URL` | `http://app-server:8090` | empty disables (open mode) |
| nntp-proxy | `REPORT_INTERVAL_SECS` | `5` | activity report cadence |
| nntp-proxy | `LOCK_POLL_INTERVAL_SECS` | `2` | lock poll / effective lock latency |
| app-server | `DATABASE_PATH` | `/data/app-server.db` | SQLite file |
| app-server | `SESSION_TTL_SECS` | `30d` | minted session key TTL |
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

## Tests

13 tests across both binaries:

```bash
cargo test --bin nntp-proxy             # 4 UserPool unit tests
(cd app-server && cargo test)           # 5 Store unit + 4 HTTP integration
```

`nzb-nntp` (0.2.13 — in `~/Working/libs/`) gained a `trusted_fingerprint` field + FingerprintVerifier (137 tests, all green). Future bundled clients can pin the proxy's cert by fetching from `/api/fingerprint` and setting that field.

## Logs

Each service writes daily-rolled logs to `./logs/`:

```bash
tail -f logs/app-server.log.*
tail -f logs/nntp-proxy.log.*
tail -f logs/gui.log.*
tail -f logs/nzbservice-client-*.log.*
```

`docker logs` only shows the one-line startup message per container.
