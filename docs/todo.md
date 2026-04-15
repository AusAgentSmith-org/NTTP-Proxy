# Todo

Rough priority order, top = next. Scratchpad. Prune as things change.

## Recently done

- [x] nntp-proxy: TLS upstream pool, credential substitution, pass-through
- [x] CLI test client (NZBFailTest port) with env-var config + Docker
- [x] gui: Axum + plain HTML/JS — upload, queue, cancel
- [x] gui: nzb.indexarr search + grab (POST /api/grab/:id)
- [x] gui: per-user login/logout, hot-swap NNTP creds via update_servers
- [x] app-server: admin + proxy APIs, admin UI
- [x] proxy ↔ app-server: validate on AUTHINFO, per-user cap, activity
      reporting, lock poller drops sessions within ~2s
- [x] file logging via tracing-appender (one rolling file per service)
- [x] **app-server: SQLite persistence** — users, locks, bytes_total,
      total_sessions, last_seen all survive restart. Runtime
      active_sessions resets on restart by design.
- [x] **Throughput surfaced per-user** — bytes/sec derived in app-server
      from successive activity reports; admin UI renders a Rate column
      and "Conns / Max" to distinguish open TCP sockets from active
      downloading.
- [x] **Session keys** — gui exchanges password for an opaque key at the
      app-server on login; proxy sees the key as the NNTP AUTHINFO PASS.
      Real password never reaches the proxy. Admin UI lists sessions per
      user; locking a user revokes them. 30-day default TTL; background
      purge of expired keys.
- [x] **Idle upstream connection pool** — authenticated provider
      connections are parked on clean QUIT, reused on next acquire.
      Permits travel with the connection so provider cap is honoured
      across active+idle combined. 60s TTL sweep.
- [x] **Tests** — 12 tests between nntp-proxy (UserPool unit) and
      app-server (Store unit + HTTP integration via tower::oneshot).

## Known issues in the current POC

- [ ] **Gui session is single-global-server-side state.** Two browsers
      hitting the same gui share one session. Needs per-session cookies
      for real multi-client use (rare for a personal gui; low priority).
- [ ] **No web auth for end-users.** Anyone who can reach :8080 can use the
      gui. The gui sends one shared credential pair to the proxy. Need a
      proper login flow + per-user gui sessions.
- [ ] **Bytes-counted on the proxy are server→client only.** Upstream
      receive bytes aren't tracked separately. For "data used" billing
      that's likely fine; for fairness/caps it might not be.
- [ ] **Upstream connections are dropped when a client disconnects** even
      though they're still authenticated and healthy. Should be returned
      to an idle pool with a TTL.
- [ ] **No tests.** `nzb-nntp::testutil::MockNntpServer` exists; we should
      write at least one integration test for the proxy auth path and one
      for the lock-drops-session path.
- [ ] **No graceful shutdown.** SIGTERM kills in-flight transfers mid-body.
- [ ] **Cargo.lock pins registry versions, not local lib versions.** Cargo
      logs "patch was not used" warnings on every build. Expected; only
      worth fixing when local libs introduce a breaking change we need.

## Next up — pre-internet-rollout

- [ ] **NNTPS listener on the proxy (:563).** Plaintext AUTHINFO PASS is
      fine for localhost compose; once the proxy's on the public
      internet, TLS is mandatory. Self-signed cert generated at startup
      in `/data/tls/`, persists across restarts.
- [ ] **Cert fingerprint pinning in the bundled client.** We control the
      binaries — embed the expected fingerprint at build time (or fetch
      it once at install via a rendezvous URL). rustls
      `ServerCertVerifier` checks fingerprint only; no CA chain, no
      Let's Encrypt, no DNS dance.
- [ ] **Logout stops in-flight downloads.** `update_servers([])` removes
      configured servers but existing per-job workers hold connections
      until the current article finishes. Either drain via `remove_job`
      on every active job in `h_logout`, or introduce a Pause feature
      that preserves the queue for resume-on-next-login.
- [ ] **Stream multi-line bodies.** `pool.rs::read_multiline_body()`
      currently buffers the whole article (up to ~750 KB) before
      forwarding. Swap to line-by-line forwarding so peak memory per
      session drops from article size to line size.

## Near-term — make it less of a toy

- [ ] **Per-user web login on the gui.** Username/password → session token
      → API calls authenticated. Same credentials the user gets when admin
      creates them. Removes the shared-credentials hack in compose env.
- [ ] **Idle upstream connection pool.** Don't tear down + re-auth a
      provider connection every time a user reconnects. `nzb-nntp::pool`
      already does this — either expose its raw transport or copy the
      pattern.
- [ ] **TLS to clients (NNTP on :563).** Let's Encrypt at the edge or
      `tokio-rustls` server config inline. AUTHINFO PASS over plaintext is
      not OK for anything internet-facing.
- [ ] **Activity event stream.** Replace the proxy's lock-poll loop with
      SSE/long-poll on the app-server. Currently lock takes effect within
      ~2s; SSE makes it sub-100ms and removes a bunch of polling traffic.
- [ ] **Admin: edit max_connections in the UI.** API exists; UI doesn't.
- [ ] **Metrics endpoint** (Prometheus). Pool utilisation, active sessions
      per user, bytes/sec.
- [ ] **Graceful shutdown.** Stop accepting on SIGTERM, drain in-flight
      sessions with a timeout, then exit.
- [ ] **Tests.** Two priorities: (a) proxy auth → app-server validate path,
      (b) locked user has their session dropped within `LOCK_POLL_INTERVAL_SECS`.

## Scaling levers — when user load justifies it

- [ ] **Multi-account upstream.** Pool of N provider accounts; round-robin
      across them on acquire. Each account has its own connection cap.
      Doubles/triples capacity without reseller negotiation.
- [ ] **Article cache** (message-id keyed, disk-backed). Popular releases
      downloaded once, served N times. Biggest single bandwidth multiplier.
- [ ] **Regional proxies + GeoDNS.** Only if remote users complain about
      throughput. NNTP's multi-connection nature masks a lot of latency.
- [ ] **Horizontal scaling.** Multiple proxy instances behind a TCP load
      balancer + shared app-server state. Sticky sessions OR move state
      out of the proxy.

## Decisions still open

- [ ] **Reseller vs multi-account.** Contact Eweka / Astraweb / XSUsenet
      about wholesale arrangements? Or just stack consumer accounts?
      Depends on projected scale.
- [ ] **Search front-end placement.** Today the gui calls `nzb.indexarr`
      directly with the API key in env. Cleaner to proxy through
      app-server so each user gets their own indexarr key (and we can
      revoke). Trade-off: more moving parts.
- [ ] **Cert-based client auth?** TLS client certs instead of
      AUTHINFO USER/PASS. More secure but harder to configure for users.
- [ ] **Public-internet-facing or private only?** Affects choice of TLS
      strategy, auth strength, and whether we even need rate limiting.

## Client-side / UX

- [ ] The `client/` test harness still has hardcoded fallback servers in
      `build_servers()`. Remove or move behind a feature flag — they're
      noise now.
- [ ] Decide what the **actual** end-user client is. Options: keep gui as
      the default; rebrand `rustnzbd`; or just tell users to point
      SABnzbd / NZBGet at our proxy.

## Infra / deployment

- [ ] Pick deployment target (Hetzner dedicated likely).
- [ ] Push images to `repo.indexarr.net` (Forgejo container registry) once
      past toy stage. Wire up Forgejo CI.
- [ ] Move secrets to Infisical (existing pattern).
- [ ] Push log files into Loki on Node B (already running).
