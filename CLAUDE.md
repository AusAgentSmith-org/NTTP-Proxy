# nzbservice — agent briefing

> **TL;DR:** Two-binary POC of a shared-access Usenet platform. A Rust **NNTP
> proxy** brokers many clients onto one (or a few) upstream Usenet provider
> accounts. A **test client** (ported from `NZBFailTest`) drives real NZB
> downloads through the proxy. Both run under docker-compose. The broader
> system (App Server, user management, nzb.indexarr search integration) is
> **not yet built** — don't invent requirements for it, read `docs/todo.md`
> and ask.

## Where things live

```
nzbservice/
├── src/                 nntp-proxy  (Rust binary, builds in Docker)
│   ├── main.rs          TCP listener, per-client task spawn
│   ├── config.rs        env-var config (NNTP_HOST/PORT/USER/PASS/CONNECTIONS)
│   ├── pool.rs          UpstreamPool — semaphore-capped TLS connections
│   └── session.rs       per-client NNTP session state machine
├── Cargo.toml           proxy deps: tokio, tokio-rustls, tracing-appender
├── Dockerfile           rust:1.90 builder → debian:trixie runtime
├── client/              test client
│   ├── src/main.rs      NZBFailTest verbatim, env-var-configurable
│   ├── Cargo.toml       uses local path patches for private crates
│   └── Dockerfile       packages a PRE-BUILT host binary (see below)
├── docker-compose.yml   proxy + scalable client service
├── docs/
│   ├── architecture.md  system intent + proxy internals
│   └── todo.md          known issues, next steps, open decisions
├── nzbs/                user-supplied NZBs (gitignored)
├── downloads/           client output (gitignored)
└── logs/                per-service daily-rolled logs (gitignored)
```

## Critical context

### 1. Where the proxy's upstream client code comes from

**It doesn't use `nzb-nntp`.** `nzb-nntp::NntpConnection` only exposes
high-level methods (`fetch_body`, `stat_article`, `xover`, …) that return
parsed structs. A proxy needs raw byte forwarding, so `src/pool.rs` opens
its own `tokio-rustls` TLS connection to the upstream and does AUTH +
read/write directly. This is intentional; don't try to "simplify" it by
routing through `nzb-nntp`.

### 2. Why the client can't build in Docker

The client depends on `nzb-web`, `nzb-core`, `nzb-nntp`, `nzb-postproc` —
all published to the private Forgejo cargo registry at
`repo.indexarr.net` (behind Tailscale; unreachable from inside a Docker
build container). Cargo tries to update the registry index even when all
crates are `[patch]`-ed to local paths, and fails on auth.

**So the client Dockerfile packages a pre-built host binary.** Workflow:

```bash
(cd client && cargo build --release)     # on host, where Forgejo is reachable
docker compose build client              # just COPYs target/release/nzb-fail-test
```

If you need to rebuild the client after a code change, you must run `cargo
build --release` on the host first. Don't try to make the client build
inside Docker; that path was tried and doesn't work without vendoring all
private deps.

### 3. glibc alignment

Host binaries are linked against whatever glibc the host runs (≥2.38 on
modern Ubuntu/Debian). `debian:bookworm-slim` ships glibc 2.36 and **will
not run** these binaries. Both Dockerfiles use `debian:trixie-slim`
(glibc 2.41) for this reason. Don't downgrade.

### 4. Logging goes to files, not stdout

Both binaries use `tracing-appender` to write daily-rolled logs to
`/logs/<name>.log.<date>`. The `./logs/` volume mount surfaces these on
the host. `docker logs <service>` only shows the one-line startup
message — tail the actual file for detail:

```bash
tail -f logs/nntp-proxy.log.*
tail -f logs/nzbservice-client-1.log.*
```

Don't restore stdout logging without asking — the user explicitly removed
it as too noisy.

### 5. Local lib versions are out of sync with registry versions

The client's `Cargo.lock` pins registry versions (e.g. `nzb-web 0.1.10`)
while the local libs at `~/Working/libs/*` are further ahead (e.g.
`nzb-web 0.2.0`). Cargo therefore uses the registry versions and prints
"patch was not used" warnings. **This is expected.** Running `cargo
update` to force local versions is an explicit decision — don't do it
reactively to clear warnings.

## NNTP protocol knowledge needed

The proxy forwards arbitrary NNTP commands byte-for-byte. Multi-line
response detection relies on the status code:

- **Multi-line** (body follows, terminated by `.\r\n`):
  `215`, `220`, `221`, `222`, `224`, `225`, `230`, `231`, `282`
- **Single-line:** everything else

This set is in `src/session.rs::is_multiline()`. If you add support for a
new NNTP extension whose response is multi-line, add its code there.

## Tech stack summary

| | Version | Notes |
|---|---|---|
| Rust | 2024 edition, rustc ≥1.88 required (time crate) | |
| Docker builder | `rust:1.90-slim` | trixie-based |
| Docker runtime | `debian:trixie-slim` | must stay trixie for glibc |
| TLS | `tokio-rustls 0.26` + `webpki-roots 1` | matches `nzb-nntp` |
| Async | `tokio 1` full features | |
| Logging | `tracing` + `tracing-appender` daily | |

## Commands you'll need

```bash
# Full rebuild + run
cd ~/Working/apps/nzbservice
(cd client && cargo build --release)         # only if client code changed
docker compose build
docker compose up --scale client=2

# Watch what's happening
tail -f logs/nntp-proxy.log.*
tail -f logs/nzbservice-client-*.log.*

# Rebuild proxy only (fast — builds in Docker)
docker compose build nntp-proxy

# Stop everything
docker compose down
```

## What NOT to do

- **Don't add `Co-Authored-By: Claude` or similar to commits** — `~/Working/CLAUDE.md` forbids it.
- **Don't try to make the client build inside Docker.** Forgejo is unreachable; pre-building on host is the chosen path.
- **Don't downgrade the Docker base images.** trixie is required for glibc.
- **Don't commit `target/`, `nzbs/`, `downloads/`, or `logs/`** — they're gitignored for a reason.
- **Don't implement user authentication by inventing requirements.** Read `docs/todo.md` and ask about the App Server design.
- **Don't restore stdout logging** without explicit user request.
- **Don't "fix" the Cargo.lock warnings about unused patches** — they're expected, see §5.

## Where to look first when something breaks

| Symptom | Start here |
|---|---|
| Proxy compiles but nothing connects | `src/session.rs` welcome banner + auth flow |
| BODY returns truncated data | `is_multiline()` missing the response code? |
| Client immediately exits in Docker | glibc mismatch — check runtime base image |
| `registry index was not found` in Docker | Something reintroduced a Forgejo registry dep in the proxy path |
| `authenticated registries require a credential-provider` | Docker build is trying to reach Forgejo — only happens for the client, must be pre-built |
| Pool acquire hanging | Upstream is dead or auth is failing; grep `acquire upstream slot` in proxy log |

## Tone notes

The user works fast and prefers concise output. Don't write walls of
text, don't restate what they just said, don't list things they didn't
ask about. Architectural conversations happen in prose, not bullet
points. Confirm decisions, don't recommend ten options.
