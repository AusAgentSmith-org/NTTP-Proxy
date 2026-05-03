# nzbservice

A multi-user Usenet platform built as a proof-of-concept. The core idea: one upstream provider account is shared across multiple users via an NNTP proxy that handles authentication, per-user connection caps, session management, and instant revocation — all without exposing the real provider credentials to any client.

Users log in through a web UI, get a short-lived session key, and that key is what the NNTP client actually uses. The proxy validates every connection against the auth server. Lock a user and their sessions drop within ~2 seconds.

## Services

Four cooperating services under one `docker compose`:

| Service | Port(s) | Role |
|---|---|---|
| **`app-server`** | `:8090` | Source of truth. Users, session keys, connection caps, lock flags, activity tracking. SQLite-backed. Admin UI included. |
| **`nntp-proxy`** | `:1119` plain, `:5563` NNTPS | Accepts NNTP connections from clients, proxies upstream over TLS. Validates auth, enforces caps, reuses idle upstream connections. Self-signed cert with fingerprint endpoint for client pinning. |
| **`gui`** | `:8080` | Web client. Login, NZB upload, download queue with per-job cancel, search via nzb.indexarr with one-click grab. |
| **`client`** | — | Headless CLI client. Same download engine as the GUI. Useful for load testing with `--scale client=N`. |

```
                    ┌──────────────────┐
      browser ◄───► │    app-server    │ :8090
      admin UI      │  users · locks · │
                    │  sessions · FP   │
                    └─┬────────▲───────┘
                      │ login   │ validate / activity / locked?
                      │ session │
                      │ key     │
       ┌──────────────┘         │
       │                        │
       ▼                        │
    ┌──────┐                   ┌┴─────────────┐       ┌──────────┐
    │ gui  │ :8080 ──NNTP─────►│  nntp-proxy  │──TLS─►│ provider │
    │      │                   │ :1119  :5563 │       │          │
    └──────┘                   └──────────────┘       └──────────┘
    ┌──────┐  ──NNTP──────────►
    │client│
    └──────┘
```

## Auth flow

1. User logs into the **gui** with `username + password`.
2. Gui requests a **session key** from the app-server (48-char random, 30-day TTL).
3. Gui uses `username + session_key` as the NNTP credentials — the real password never leaves the GUI.
4. On each `AUTHINFO PASS`, the proxy asks the app-server to validate the key and returns the user's connection cap.
5. Locking a user in admin revokes all their keys and drops active NNTP sessions within ~2s.

## Connection caps

Two semaphores stack per session:

- **Pool-wide** — sized to the provider account's total connection limit. Counts both active and idle upstream sockets.
- **Per-user** — sized to that user's `max_connections`. Set per-user in the admin UI.

A new session acquires per-user first, then pool-wide. Idle upstream connections are reused for up to 60s before being dropped.

## Setup

`gui` and `client` depend on the private Forgejo cargo registry and must be pre-built on the host. `app-server` and `nntp-proxy` build entirely inside Docker.

```bash
cp .env.sample .env
# Edit .env — set your provider credentials and change the tokens

mkdir -p nzbs downloads logs data
(cd gui    && cargo build --release)
(cd client && cargo build --release)
docker compose build
docker compose up -d
```

- **GUI**: http://localhost:8080 — log in with the bootstrap credentials from `.env`
- **Admin**: http://localhost:8090 — paste `ADMIN_TOKEN`, manage users and view live activity
- **NNTPS**: `openssl s_client -connect localhost:5563` — fingerprint at `GET /api/fingerprint`

The bootstrap user is created on first app-server start only if the database is fresh.

## Configuration

Copy `.env.sample` to `.env` and fill in:

| Variable | Purpose |
|---|---|
| `INDEXARR_URL` / `INDEXARR_API_KEY` | NZB search backend |
| `ADMIN_TOKEN` | Admin UI bearer token |
| `PROXY_TOKEN` | Proxy ↔ app-server bearer token |
| `BOOTSTRAP_USER/PASS` | Initial user, created if DB is empty |
| `BOOTSTRAP_MAX_CONNECTIONS` | That user's connection cap |

Provider and tuning knobs live in `docker-compose.yml`:

| Service | Variable | Default | Notes |
|---|---|---|---|
| nntp-proxy | `NNTP_HOST/PORT/USER/PASS` | — | upstream provider |
| nntp-proxy | `NNTP_CONNECTIONS` | `15` | provider account cap |
| nntp-proxy | `TLS_PORT` | `563` | set to `0` to disable NNTPS |
| nntp-proxy | `APP_SERVER_URL` | `http://app-server:8090` | empty = open mode (no auth) |
| nntp-proxy | `LOCK_POLL_INTERVAL_SECS` | `2` | lock enforcement latency |
| app-server | `SESSION_TTL_SECS` | `2592000` | 30 days |
| gui | `MAX_ACTIVE_DOWNLOADS` | `2` | concurrent download jobs |

## Rebuild

```bash
# Proxy or app-server (build in Docker):
docker compose build nntp-proxy && docker compose up -d nntp-proxy
docker compose build app-server && docker compose up -d app-server

# GUI (pre-build on host first):
(cd gui && cargo build --release) && docker compose build gui && docker compose up -d gui

# CLI client:
(cd client && cargo build --release) && docker compose build client
docker compose up -d --scale client=2
```

## Tests

```bash
cargo test --bin nntp-proxy      # 4 UserPool unit tests
(cd app-server && cargo test)    # 5 Store unit + 4 HTTP integration
```

## Logs

Each service writes daily-rolled logs to `./logs/` — `docker logs` only shows the startup line.

```bash
tail -f logs/app-server.log.*
tail -f logs/nntp-proxy.log.*
tail -f logs/gui.log.*
```

## Layout

```
nzbservice/
├── src/                    nntp-proxy
│   ├── main.rs             listeners + background tasks
│   ├── pool.rs             upstream TLS pool, idle reuse, TTL sweep
│   ├── session.rs          NNTP state machine (generic over transport)
│   ├── user_pool.rs        per-user semaphore + cancellable session registry
│   ├── app_client.rs       HTTP client → app-server
│   ├── tls.rs              self-signed cert generation + persistence
│   └── config.rs           env-driven config
├── app-server/
│   └── src/
│       ├── handlers.rs     HTTP handlers (admin, proxy auth, fingerprint)
│       ├── store.rs        SQLite-backed user/session store
│       ├── state.rs        AppState + Config
│       └── main.rs         router wiring + background tasks
├── gui/                    web client (pre-built host binary + static assets)
├── client/                 CLI test harness (pre-built host binary)
├── docker-compose.yml
├── .env.sample
└── data/                   SQLite DB + TLS cert (gitignored)
```
