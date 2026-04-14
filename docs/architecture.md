# Architecture

## Target system

```
                    ┌───────────────────┐
                    │    Client App     │
                    └────┬────┬─────┬───┘
           NNTP (proxied) │    │     │  HTTP
                          │    │     │
  ┌───────────────────────▼─┐  │     └─────────────┐
  │   UsenetServer Proxy    │  │  HTTP            ▼
  │  (this repo)            │  │        ┌──────────────────────┐
  │  • per-user auth        │  │        │  nzb.indexarr        │
  │  • outbound pool to     │  │        │  (NZB search)        │
  │    provider accounts    │  │        └──────────┬───────────┘
  │  • usage metering       │  │                   │
  └──────────┬──────────────┘  │                   │ credential
             │ NNTP/TLS        │                   │ provisioning
             ▼                 │                   │
    ┌─────────────────┐        │                   │
    │ Usenet Provider │        ▼                   ▼
    └─────────────────┘   ┌─────────────────────────────────┐
                          │          App Server             │
                          │  • user auth / sessions         │
                          │  • issue nzb.indexarr API keys  │
                          │  • per-user NNTP credentials    │
                          │  • track per-user usage (GB,    │
                          │    connections, active sessions)│
                          └─────────────────────────────────┘
```

**Intent.** End users consume Usenet (search + download) through infrastructure
we control. Upstream Usenet provider accounts are owned by us, not users — so
most Usenet providers' "no account sharing" terms are a constraint we design
around, either via commercial reseller arrangements or a pool of consumer
accounts behind the proxy. Either way, from the provider's perspective, all
traffic comes from our infrastructure.

## Scope of the POC

Only the **UsenetServer Proxy** component exists today. Everything else (App
Server, `nzb.indexarr` integration, user management, billing) is deferred.

```
nzbservice/
├── src/            nntp-proxy binary (Rust, tokio, tokio-rustls)
├── client/         test client — NZBFailTest ported to env-var config
└── docker-compose.yml
```

The POC proves out the **mechanics** of NNTP proxying: accept plaintext NNTP
from clients, authenticate upstream via TLS with our credentials, forward
commands transparently, share one upstream account across many clients.

## Proxy internals

One session per client TCP connection. Two phases.

**Phase 1 — auth handshake.** The proxy speaks NNTP directly. Client credentials
are accepted but not (yet) validated against a user database — today any
`AUTHINFO PASS` triggers a pool acquire.

```
← client connects
→ 200 nntp-proxy ready
← AUTHINFO USER alice
→ 381 Password required
← AUTHINFO PASS *
  [acquire upstream slot — creates TLS connection to provider,
   authenticates with our credentials, connection is now Ready]
→ 281 Authentication accepted
```

**Phase 2 — pass-through.** Every command and response is forwarded byte-for-byte.
The proxy reads one response line, parses the status code, and if it's a
multi-line response (220, 222, 224, …) reads until `.\r\n` and forwards the
whole body.

```
← BODY <message-id@host>
→ BODY <message-id@host>            (to upstream)
← 222 0 <message-id> article data   (from upstream)
← <~750 KB of yEnc>                 (from upstream, multi-line)
← .\r\n
→ forward all to client
```

### Connection pool

`Arc<tokio::sync::Semaphore>` capped at `NNTP_CONNECTIONS`. Each client session
holds one permit for its lifetime, so N client TCP connections require N
upstream connections (up to the cap). Connections are created on demand — no
pre-warming — and dropped when the client disconnects.

### What the proxy does *not* do yet

- **User authentication.** AUTHINFO PASS is accepted without validation.
- **Article caching.** Two clients downloading the same NZB each fetch the
  same articles — no de-duplication.
- **Usage tracking.** No metering of GB/connections per client.
- **TLS to clients.** Clients connect in plaintext. Fine on a private
  network; not fine over the internet.
- **Connection reuse across sessions.** Upstream connections are closed when
  the client disconnects, even though the `NntpConnection` is still authenticated
  and healthy.

## Technical choices

| Area | Choice | Reason |
|---|---|---|
| Language | Rust | Matches surrounding stack (`nzb-*` crates, `Arz`). No GC pauses on a long-running proxy. |
| Async runtime | Tokio | Standard in the stack. |
| TLS | `tokio-rustls` + `webpki-roots` | Same deps as `nzb-nntp`, no OpenSSL. |
| Upstream protocol | Raw TLS + hand-rolled NNTP I/O | `nzb-nntp::NntpConnection` has no raw send/recv API; its high-level methods (`fetch_body` etc.) return parsed structs that would need re-encoding. Raw I/O is simpler and avoids re-stuffing dot-escapes on the forwarding path. |
| Client protocol | Plain TCP | POC scope; TLS termination is trivial to add later with `tokio-rustls` server mode. |
| Logging | `tracing` + `tracing-appender` daily rolling | Each container gets its own log file in `/logs/`; stdout stays clean so `docker logs` isn't a firehose. |

## Deployment shape (target)

Single Hetzner dedicated box is the current thinking, with high/unmetered
bandwidth. Revisit once real user count and per-user usage are known.

- **Proxy:** one instance, high-bandwidth egress box. Client connections are
  plaintext internal to the datacentre, or behind a TLS termination at the edge.
- **App Server:** separate Hetzner VM. Stateful (postgres), lower traffic.
- **`nzb.indexarr`:** existing Indexarr deployment.

Global routing (users outside EU) is **deferred**. NNTP's multi-connection
nature masks most of the latency penalty for remote users. Add regional proxies
via GeoDNS only when a specific region generates enough traffic to justify it.

## Bandwidth reality check

Rough math for sizing the egress box:

- 1 heavy user sustained ≈ 150–300 Mbit/s while downloading
- 20 TB/month VPS ≈ 60 Mbit/s sustained → **covers ~1 heavy user**
- Unmetered Hetzner dedicated (1 Gbit/s typical) → **covers ~5–7 heavy users sustained**

Any serious deployment needs either unmetered bandwidth OR many small regional
proxies sharing the load OR article caching (a popular release downloaded once
from the provider serves N users).
