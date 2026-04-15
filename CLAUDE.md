# nzbservice — agent briefing

> **TL;DR:** Four cooperating Rust services run under one `docker compose`:
> `app-server` (user/lock/activity, :8090), `nntp-proxy` (NNTP broker, :1119),
> `gui` (web UI + nzb.indexarr search, :8080), `client` (CLI test harness).
> The proxy validates every NNTP login against the app-server, enforces a
> per-user connection cap, reports activity every 5s, and drops sessions
> within ~2s when a user is locked. Read `docs/architecture.md` for the
> shape and `docs/todo.md` for what's known-bad / next.

## Where things live

```
nzbservice/
├── README.md  CLAUDE.md  docs/{architecture,todo}.md
├── src/                 nntp-proxy
│   ├── main.rs          listener; spawns activity reporter + lock poller
│   ├── config.rs        env config (NNTP_*, APP_SERVER_URL, PROXY_TOKEN, …)
│   ├── pool.rs          UpstreamPool — TLS connection pool to provider
│   ├── user_pool.rs     per-user semaphore + session registry (cancel chans)
│   ├── app_client.rs    HTTP client for app-server (validate / activity / locked)
│   └── session.rs       per-client NNTP state machine (auth + pass-through)
├── Cargo.toml           proxy deps: tokio, tokio-rustls, reqwest, parking_lot
├── Dockerfile           rust:1.90 builder → debian:trixie runtime
├── app-server/          user + lock + activity service
│   ├── src/{main,handlers,state,store}.rs
│   ├── static/          admin HTML/JS/CSS
│   └── Dockerfile       builds in Docker (public crates only)
├── gui/                 web client
│   ├── src/main.rs      Axum routes: queue, upload, search, grab, cancel
│   ├── static/          HTML/JS/CSS, polls /api/queue every 2s
│   └── Dockerfile       packages PRE-BUILT host binary + static assets
├── client/              CLI test harness
│   ├── src/main.rs      NZBFailTest port, env-var driven
│   └── Dockerfile       packages PRE-BUILT host binary
├── docker-compose.yml   all four services
├── .env                 INDEXARR_API_KEY, ADMIN_TOKEN, PROXY_TOKEN, BOOTSTRAP_*
├── nzbs/                drop .nzb files here for the CLI client (gitignored)
├── downloads/           gui + client write here (gitignored)
└── logs/                per-service rolling daily logs (gitignored)
```

## Service roles in one paragraph each

**`app-server`** — single source of truth for users (`username`,
salted-SHA256 password, `max_connections`, `locked`) and proxy-reported
runtime state (`active_sessions`, `bytes_total`, `last_seen`). In-memory
store; restart loses state. Two HTTP surfaces, both bearer-token:
`/api/admin/*` (ADMIN_TOKEN, used by admin HTML UI) and `/api/proxy/*`
(PROXY_TOKEN, used by nntp-proxy: validate, activity, locked).
Bootstrap user is created from `BOOTSTRAP_USER`/`BOOTSTRAP_PASS` env on
first start.

**`nntp-proxy`** — accepts plaintext NNTP on :119, opens a TLS connection
to one upstream Usenet server using a single set of credentials (env). On
client `AUTHINFO PASS`: calls `app-server /api/proxy/validate`; if
allowed, acquires a per-user semaphore permit (cap = `max_connections`
from app-server) and an upstream provider permit. Two background tokio
tasks: activity reporter (every 5s, drains UserPool counters → POST) and
lock poller (every 2s, GET `/locked`, trip cancel on each active session
of locked users).

**`gui`** — Axum + plain HTML/JS. Reuses `nzb-web::QueueManager` for
downloading (same engine as the CLI client). Three tabs: Queue (polls,
cancel button), Upload (drag/drop multipart), Search (proxies to
nzb.indexarr `/api/releases`, "↓" button does `/api/releases/:id/nzb` →
parse → enqueue).

**`client`** — Headless port of `NZBFailTest`. Reads `.nzb` files from
`/nzbs`, env-var driven, useful for repeatable load tests. Scale with
`--scale client=N`.

## Critical context

### 1. Where the proxy's upstream code comes from

**Not `nzb-nntp`.** `NntpConnection` only exposes high-level methods
(`fetch_body`, `xover`, …) that return parsed structs; a proxy needs raw
byte forwarding. `src/pool.rs` opens its own `tokio-rustls` TLS
connections. Don't try to "simplify" by routing through `nzb-nntp`.

### 2. Why `gui` and `client` can't build in Docker

They depend on `nzb-web`, `nzb-core`, `nzb-nntp`, `nzb-postproc`,
`nzb-decode`, `yenc-simd` — all on the private Forgejo cargo registry at
`repo.indexarr.net` (Tailscale, unreachable from Docker). Cargo
unconditionally tries to update the registry index before applying
`[patch]` sections, and fails on auth.

**Both Dockerfiles package a PRE-BUILT host binary.** Workflow:

```bash
(cd gui    && cargo build --release)
(cd client && cargo build --release)
docker compose build {gui,client}
```

`app-server` and `nntp-proxy` use only public crates — they build inside
Docker as normal.

### 3. glibc alignment

Host binaries link against host glibc (≥2.38 on modern Ubuntu/Debian).
`debian:bookworm-slim` ships glibc 2.36 — incompatible. **All Dockerfiles
use `debian:trixie-slim`** (glibc 2.41). Don't downgrade.

### 4. Logs go to files, not stdout

Every service uses `tracing-appender` to write daily-rolled logs to
`/logs/<name>.log.<date>`. The `./logs/` volume mount surfaces them on
the host. `docker logs <service>` only carries the one-line startup
message — tail the file for detail:

```bash
tail -f logs/app-server.log.*
tail -f logs/nntp-proxy.log.*
tail -f logs/gui.log.*
tail -f logs/nzbservice-client-*.log.*
```

Don't restore stdout logging without explicit user request.

### 5. Local lib versions diverge from registry versions

The `gui`/`client` `Cargo.lock` pins registry versions (e.g. `nzb-web
0.1.10`); the local libs at `~/Working/libs/*` are ahead (e.g. `0.2.x`).
Cargo prints `patch was not used` warnings on every host build. **This
is expected.** `cargo update` to force local versions is a deliberate
choice — don't do it reactively.

### 6. App-server state is in-memory

Every restart creates a fresh app-server: bootstrap user re-created,
all created users gone, all activity counters reset. Persistence is in
`docs/todo.md` — don't bolt on without a chat first.

### 7. Per-user vs pool-wide connection caps

Two semaphores stack:

- **Pool-wide** (`pool.rs`) — sized to provider account limit (`NNTP_CONNECTIONS`,
  default 15). Caps total concurrent upstream sockets.
- **Per-user** (`user_pool.rs`) — sized to that user's `max_connections`
  from the app-server. Caps how many sessions one user can run.

A new session acquires the per-user permit first, then the pool-wide
permit. Both must be available.

### 8. Lock enforcement is poll-based, not push

Lock takes effect within `LOCK_POLL_INTERVAL_SECS` (default 2s). The
proxy polls `/api/proxy/locked`, finds matches in its session registry,
fires the per-session `oneshot::Sender<()>` cancel signal, the session
selects on it and exits with `482 Account locked`. Switching to SSE for
sub-100ms locks is in `todo.md`.

## NNTP protocol facts the proxy depends on

The proxy forwards arbitrary commands byte-for-byte. Multi-line response
detection is by status code:

- **Multi-line** (body follows, terminated by `.\r\n`):
  `215`, `220`, `221`, `222`, `224`, `225`, `230`, `231`, `282`
- **Single-line:** everything else

Set lives in `src/session.rs::is_multiline()`. Add new codes there if a
new NNTP extension's response is multi-line.

## Tech stack

| | Version |
|---|---|
| Rust | 2024 edition; rustc ≥1.88 (time crate) |
| Docker builder | `rust:1.90-slim` (trixie-based) |
| Docker runtime | `debian:trixie-slim` (glibc 2.41) |
| HTTP server | `axum 0.8` |
| HTTP client | `reqwest 0.12` (rustls only, no openssl) |
| TLS | `tokio-rustls 0.26` + `webpki-roots 1` |
| Async | `tokio 1` full features |
| Locks | `parking_lot::{Mutex, RwLock}` (sync code paths only) |
| Logging | `tracing` + `tracing-appender` daily rolling |

## Commands

```bash
cd ~/Working/apps/nzbservice

# Full build + run
(cd gui && cargo build --release)
(cd client && cargo build --release)
docker compose build
docker compose up -d

# UIs
xdg-open http://localhost:8080  # gui
xdg-open http://localhost:8090  # admin (token: ADMIN_TOKEN from .env)

# Watch what's happening
tail -f logs/*

# Fast iterations
docker compose build nntp-proxy && docker compose up -d nntp-proxy
docker compose build app-server && docker compose up -d app-server
(cd gui && cargo build --release) && docker compose build gui && docker compose up -d gui

# Stop everything
docker compose down
```

## Quick smoke tests

```bash
# Health
curl http://localhost:8090/health
curl -o /dev/null -w "%{http_code}\n" http://localhost:8080/

# Admin lists users
curl -H "Authorization: Bearer admin-dev-token" http://localhost:8090/api/admin/users

# NNTP auth (bad password should 481)
{ printf 'AUTHINFO USER guiuser\r\nAUTHINFO PASS WRONG\r\nQUIT\r\n'; sleep 1; } | nc -w 3 localhost 1119

# Lock a user, then try to authenticate as them — should 481
curl -H "Authorization: Bearer admin-dev-token" -H "Content-Type: application/json" \
  -X PUT -d '{"locked":true}' http://localhost:8090/api/admin/users/guiuser/lock
```

## What NOT to do

- **No `Co-Authored-By: Claude` etc. on commits** — `~/Working/CLAUDE.md` forbids it.
- **Don't try to make `gui`/`client` build in Docker.** Forgejo is unreachable; pre-build on host.
- **Don't downgrade Docker base images.** trixie required for glibc.
- **Don't commit `target/`, `nzbs/`, `downloads/`, `logs/`, `.env`** — gitignored.
- **Don't restore stdout logging** without explicit user request.
- **Don't `cargo update` to clear "patch not used" warnings** without a reason.
- **Don't add persistence/web-auth to app-server casually** — chat first; user explicitly scoped this iteration as POC with in-memory state.
- **Don't invent users.** Ask before creating fake-data flows.

## Where to look first when something breaks

| Symptom | Start here |
|---|---|
| GUI loads but downloads never start | Check `logs/nntp-proxy.log.*` — usually upstream auth or pool exhausted |
| All AUTH fails with 481 | Is app-server up? `docker compose ps app-server`; check `PROXY_TOKEN` matches in both services |
| User can authenticate but only N sessions work | Their `max_connections` cap. Bump in admin UI |
| Lock doesn't take effect | Lock poller running? Should see "dropped sessions for locked users" in proxy log within 2s |
| BODY data truncated | `is_multiline()` missing the response code? |
| Client immediately exits in Docker | glibc mismatch — runtime image must be trixie |
| `registry index was not found` building proxy | Something added a Forgejo registry dep to the proxy's Cargo.toml |
| `authenticated registries require a credential-provider` | gui/client trying to build inside Docker — must be pre-built on host |
| Pool acquire hangs forever | Upstream is dead OR every per-user slot is held — grep `acquire upstream slot` |
| Admin UI says "unauthorised" | Wrong `ADMIN_TOKEN`. Click "Forget token" and re-paste |

## Tone notes

User prefers concise output. No walls of text, no restating what they
just said, no bullet lists they didn't ask for. Architectural
conversations happen in prose. Confirm decisions, don't recommend ten
options. When something works, say so briefly and move on.
