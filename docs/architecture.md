# Architecture

## Where we are

All four pieces of the originally-sketched system exist now:

```
                        ┌─────────────────────────┐
                        │       App Server        │ :8090
                        │  • user accounts        │
                        │  • per-user max_conns   │
                        │  • lock control         │
                        │  • activity tracking    │
                        │  • session keys         │
                        │  • FP endpoint          │
                        │  • SQLite-persisted     │
                        └─────┬────────────┬──────┘
              /api/auth/login │            │ /api/proxy/validate
              /api/proxy/      │            │ /api/proxy/activity
                      │        │            │ /api/proxy/locked
            ┌─────────┴──────┐ │            │
            │   gui :8080    │◄┘            ▼
            │  • login/logout │    ┌──────────────────────┐
            │  • upload/queue │    │    nntp-proxy        │
            │  • search/grab  │───►│  :119 plain          │
            │  • cancel jobs  │    │  :563 NNTPS          │
            └────────────────┘     │  • TLS upstream pool │
            ┌────────────────┐     │    with idle reuse   │
            │ client (CLI)   │────►│  • per-user semapho  │
            └────────────────┘     │  • lock-on-the-fly   │
                                   └──────────┬───────────┘
                                              │ NNTP/TLS
                                              ▼
                                      ┌──────────────┐
                                      │ Usenet       │
                                      │ Provider     │
                                      └──────────────┘

              ┌──────────────────────┐
              │   nzb.indexarr       │  external — search hits here via gui
              │   (search + NZB fetch)│
              └──────────────────────┘
```

**What's deferred:** bundled end-user client binary, web auth with per-user
cookie sessions (today the gui is single-user-at-a-time with server-side
state), NNTPS-everywhere (gui still uses plain NNTP to the colocated proxy),
article caching, multi-account upstream, real deployment (Hetzner, CI,
Infisical). See `todo.md`.

## Component responsibilities

### `app-server` — source of truth

**Persistent (SQLite, `/data/app-server.db`):**

- `users` — username, salted-SHA256 password hash, max_connections,
  locked flag, created_at, and lifetime counters that accumulate across
  restarts (bytes_total, total_sessions, last_seen).
- `sessions` — session keys minted on gui login. Each row has username,
  created_at, last_used, expires_at. Indexed by `session_key` (PK) and
  `username`.

**Runtime-only (reset on restart):**

- `active_sessions` (per user): current count of live NNTP sessions,
  updated from proxy activity reports.
- `bytes_per_sec` (per user): derived from `bytes_delta / elapsed_since_last_report`.

**HTTP surfaces**, all bearer-protected except `/api/auth/*` + `/health` + `/api/fingerprint`:

- **Admin** (`ADMIN_TOKEN`):
  - `GET /api/admin/users` — list with live stats
  - `POST /api/admin/users` — create
  - `DELETE /api/admin/users/:u` — delete
  - `PUT /api/admin/users/:u/lock` — lock/unlock (lock cascades to revoke sessions)
  - `PUT /api/admin/users/:u/max_connections` — change cap
  - `GET /api/admin/users/:u/sessions` — list active session keys (prefix only)

- **Proxy** (`PROXY_TOKEN`):
  - `POST /api/proxy/validate {username, password}` — auth gate, tries the
    password as a **session key first**, falls back to raw password.
    Returns `{allowed, max_connections, auth_method}`.
  - `POST /api/proxy/activity {entries: [...]}` — periodic stats push.
  - `GET /api/proxy/locked` — list of currently-locked usernames.

- **Public**:
  - `POST /api/auth/login {username, password}` → mints 48-char session key.
  - `POST /api/auth/logout {session_key}` → revokes.
  - `GET /api/fingerprint` → proxy's NNTPS cert SHA-256 for pinning.
  - `GET /health` → "ok".

**Background tasks** (run on the same tokio runtime):

1. Decay `bytes_per_sec` to 0 when no activity report for 15s (every 5s).
2. Purge expired session keys (every 5 min).

### `nntp-proxy` — credential-substituting connection broker

Two TCP listeners:

- `:119` (plain) — back-compat for in-compose clients.
- `:563` (TLS, NNTPS) — public-facing. Self-signed cert generated at first
  start via rcgen, persisted to `/data/tls/{cert,key}.pem`. Fingerprint
  written to `/data/tls/fingerprint` for the app-server to serve.

Sessions from either listener feed one generic `session::handle` — bound
is just `AsyncRead + AsyncWrite + Unpin + Send`.

**Per session:**

1. **Auth phase.** `AUTHINFO USER` caches the name. `AUTHINFO PASS` calls
   `app-server /api/proxy/validate`. On accept, acquire a per-user
   semaphore permit (cap = `max_connections` from the response), then
   acquire an upstream provider connection.
2. **Pass-through.** Forward each client command to upstream, forward
   response back. Multi-line responses (`220/222/224/…`) forwarded
   byte-for-byte until `.\r\n`.
3. **On clean QUIT**, the upstream connection goes back to the idle pool
   for reuse. Any other exit path discards the upstream conn (protocol
   state unknown).

**Pool topology:**

- **Pool-wide semaphore** (`NNTP_CONNECTIONS`, default 15) caps total
  upstream TCP sockets — matches what the provider sees.
- **Per-user semaphore** caps a single user's concurrent sessions
  (`max_connections` from app-server). A new session must get both permits.
- **Idle pool** holds authenticated streams across sessions. Permits travel
  with idle entries so the provider cap is honoured across active + idle.
- **60s TTL sweep** drops idle conns older than 60s every 10s to avoid
  growing the pool unbounded after a burst.

**Background tasks:**

1. **Activity reporter** (every `REPORT_INTERVAL_SECS`, default 5s):
   drains per-user counters from UserPool, POSTs to `/api/proxy/activity`.
2. **Lock poller** (every `LOCK_POLL_INTERVAL_SECS`, default 2s):
   GETs `/api/proxy/locked`, fires the oneshot cancel on every live
   session for any locked user → session exits with `482 Account locked`.

### `gui` — web client

Axum + plain HTML/JS/CSS. Tabs: Queue, Upload, Search.

- **Login/logout**: password → app-server session key → stored server-side.
  The session_key is used as NNTP `AUTHINFO PASS` — the real password never
  reaches the proxy.
- **Auto-login** on startup if `BOOTSTRAP_USER/PASS` env is set.
- **Session model**: single global `RwLock<Option<Session>>` (POC scope —
  not per-browser-tab). Multiple tabs share state. Switching user logs the
  previous one out.
- **Auth gate**: axum middleware on `/api/{queue,upload,jobs,search,grab}`.
  401s anywhere flip the frontend back to the login screen via a fetch
  wrapper.
- **Download engine**: reuses `nzb-web::QueueManager`, same as the CLI
  client. `update_servers` is how new NNTP creds get installed on login —
  no QueueManager rebuild required.

### `client` — CLI test harness

Port of NZBFailTest. Reads NZBs from `/nzbs`, filters by `NZB_FILTER`,
downloads. Useful for repeatable load tests; scale with `--scale client=N`.
Doesn't use session keys — authenticates with password directly (the
app-server's validate handler still accepts that path).

## Transport

| Hop | Protocol | Auth |
|---|---|---|
| browser → gui | HTTP :8080 (compose) | cookie would come from web session; today it's global state |
| gui → app-server | HTTP :8090 | bearer (`PROXY_TOKEN`) for proxy endpoints; none for `/api/auth/*` |
| gui → nntp-proxy | plain NNTP :119 | `username + session_key` via AUTHINFO |
| client → nntp-proxy | plain NNTP :119 | `username + password` via AUTHINFO (test harness only) |
| *future* bundled client → nntp-proxy | NNTPS :563 with **fingerprint pinning** | `username + session_key` |
| nntp-proxy → provider | NNTPS :563 | shared provider account (env) |
| nntp-proxy → app-server | HTTP :8090 | bearer (`PROXY_TOKEN`) |

The NNTPS listener on the proxy uses a self-signed cert — `nzb-nntp 0.2.13`
added a `trusted_fingerprint` field so any consumer can pin it without
WebPKI / Let's Encrypt / DNS.

## Deployment shape (target — unchanged from earlier docs)

Single Hetzner dedicated box, high/unmetered bandwidth. app-server +
nntp-proxy + gui all colocated. Revisit once user count is real.

## Bandwidth reality (unchanged)

- 1 heavy user sustained ≈ 150–300 Mbit/s while downloading
- 20 TB/month VPS ≈ 60 Mbit/s sustained → covers ~1 heavy user
- Unmetered Hetzner dedicated → covers ~5–7 heavy users sustained

Article caching by message-id is the single biggest bandwidth multiplier
on the roadmap (popular release fetched once, served N times).

## Tests

**13 passing.**

- `nntp-proxy` (4): UserPool acquire/release/cap/cancel/drain semantics.
- `app-server` (9): Store unit tests for CRUD, lock, sessions, persistence
  across reopen, rate derivation. Integration tests via
  `tower::ServiceExt::oneshot` for login → validate → lock-revoke chain,
  wrong-password 401, admin-requires-token 401, fingerprint endpoint 503/200.

Plus `nzb-nntp`'s 137 tests (including 2 new ones for the fingerprint
verifier) live on the shared lib.
