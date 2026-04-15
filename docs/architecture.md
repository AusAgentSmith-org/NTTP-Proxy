# Architecture

## Where we are

The original target system has four pieces: a Client App, an NZB search service,
a UsenetServer Proxy, and a central App Server. **All four exist now**, with
caveats:

```
                        ┌─────────────────────────┐
                        │       App Server        │ :8090
                        │  • user accounts        │
                        │  • per-user max_conns   │
                        │  • lock control         │
                        │  • activity tracking    │
                        │  • admin UI             │
                        └─────┬────────────┬──────┘
                  validate    │            │   activity / locked
                       (proxy)│            │   (proxy)
            ┌──────────────────┴───┐    ┌──┴────────┐
            │  Client App (gui)     │    │ nntp-proxy │ :1119
            │  :8080                │    │  • TLS pool│
            │  • upload nzb         │    │  • per-user│
            │  • queue + cancel     │    │    cap     │
            │  • search/grab via    │    │  • lock-on-│
            │    nzb.indexarr       │    │    fly drop│
            └──────────────────┬───┘    └─────┬──────┘
                               │ NNTP :119     │ NNTP/TLS :563
                               └───────┬───────┘
                                       ▼
                               ┌──────────────┐
                               │   Usenet     │
                               │   Provider   │
                               └──────────────┘
                                       │
                                  search / grab
                                       │
                            ┌──────────▼──────────┐
                            │ nzb.indexarr        │  external — already deployed
                            │ (search + nzb fetch)│
                            └─────────────────────┘
```

**What's still notional / deferred**

- App-server state is **in-memory** — restart loses users.
- Auth between gui ↔ app-server: **not implemented** for end-users; the gui hardcodes a single set of credentials from env. Admin uses a shared `ADMIN_TOKEN`. Per-user web login is a TODO.
- Article cache, multi-account upstream, regional proxies — all deferred. See `todo.md`.

## Component responsibilities

### `app-server` — source of truth

In-memory `HashMap<username, User>` behind a `parking_lot::RwLock`. Each user
has `username`, salted-SHA256 `password_hash`, `max_connections`, `locked`, and
runtime fields populated from proxy activity reports (`active_sessions`,
`bytes_total`, `last_seen`).

Two HTTP surfaces, both bearer-authed:

- **Admin** (`ADMIN_TOKEN`) — list/create/delete users, lock/unlock,
  set max_connections, view activity. Used by the admin HTML UI.
- **Proxy** (`PROXY_TOKEN`) — `validate(username, password) → {allowed, max_connections}`,
  `activity(entries[]) → ok`, `locked() → [usernames]`. Used by the proxy.

A bootstrap user (`BOOTSTRAP_USER`/`BOOTSTRAP_PASS`) is created on first start
so the gui + client work without any manual setup.

### `nntp-proxy` — credential-substituting connection broker

Two phases per client TCP connection (unchanged from before, plus auth gate):

1. **Auth.** `AUTHINFO USER` caches the username, `AUTHINFO PASS` calls
   `app-server /api/proxy/validate`. On reject → `481`. On accept → acquire
   a per-user semaphore permit (cap = `max_connections` from app-server),
   then acquire an upstream provider connection (TLS + AUTH using the
   shared `NNTP_USER`/`NNTP_PASS` from env).
2. **Pass-through.** Forward client commands → upstream, read upstream
   response, forward back. Multi-line responses (`220`, `222`, `224`, …)
   forwarded byte-for-byte until `.\r\n`.

**Two background tasks** added for app-server integration:

- **Activity reporter** (every `REPORT_INTERVAL_SECS`, default 5s) drains
  per-user counters from `UserPool` and POSTs to `/api/proxy/activity`.
- **Lock poller** (every `LOCK_POLL_INTERVAL_SECS`, default 2s) GETs
  `/api/proxy/locked` and trips the cancel signal on every active session
  for any locked user. Sessions exit cleanly with `482 Account locked`.

Run with `APP_SERVER_URL` empty for **open mode** — no auth, no caps, no
reporting. Used for local debugging.

#### Per-user pool

`user_pool.rs` holds a `HashMap<username, UserSlot>`. Each slot has a
`tokio::sync::Semaphore` sized to `max_connections`, plus a registry of
session ids → `oneshot::Sender<()>` for cancellation. When the cap changes,
the semaphore is replaced; in-flight sessions keep their old permit until
they exit.

### `gui` — web client

Axum backend, plain HTML/JS/CSS frontend. Three tabs:

- **Queue** — polls `/api/queue` every 2s, shows active + recent. Each row
  has a ✕ button → `DELETE /api/jobs/:id` → `QueueManager::remove_job`.
- **Upload** — drag/drop multi-file → `POST /api/upload` → `parse_nzb` →
  `add_job`.
- **Search** — `GET /api/search?q=…` proxies to `nzb.indexarr` `/api/releases`.
  Each result has a ↓ button → `POST /api/grab/:id` → fetches the NZB from
  indexarr, parses, enqueues.

Reuses `nzb-web::QueueManager` for the actual download work — same engine
the CLI client uses.

### `client` — CLI test harness

Headless port of NZBFailTest. Reads NZBs from `/nzbs`, processes per
`NZB_FILTER`. Used for repeatable load tests; scale arbitrarily with
`--scale client=N`.

## Networking

All three application services live on the default compose bridge network.
Only the service ports are published to the host:

| Container | Internal | Host | Reason |
|---|---|---|---|
| nntp-proxy | `:119` | `:1119` | unprivileged port on host |
| gui | `:8080` | `:8080` | direct |
| app-server | `:8090` | `:8090` | direct |

Inter-service traffic uses container DNS (`http://app-server:8090`,
`nntp-proxy:119`).

## Deployment shape (target — unchanged)

Single Hetzner dedicated box, high/unmetered bandwidth. App-server +
nntp-proxy + gui all colocated. Revisit once user count is real.

## Bandwidth reality (unchanged)

- 1 heavy user sustained ≈ 150–300 Mbit/s while downloading
- 20 TB/month VPS ≈ 60 Mbit/s sustained → covers ~1 heavy user
- Unmetered Hetzner dedicated → covers ~5–7 heavy users sustained

Article caching by message-id is the biggest single bandwidth multiplier
on the table (popular release downloaded once → served N times).
