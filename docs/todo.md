# Todo

Rough priority order, top = next. Scratchpad, not a roadmap — prune as things change.

## Known issues in the current POC

- [ ] Upstream connections are dropped when a client disconnects even though they're
      still authenticated and healthy. Should be returned to an idle pool with a
      short TTL. Otherwise every client reconnect pays a TCP+TLS+AUTH round-trip.
- [ ] `AUTHINFO PASS` is accepted without validation. Today the proxy is
      effectively open to anyone who can reach port 119.
- [ ] The proxy doesn't handle `STAT` / `HEAD` / `ARTICLE` specially — they
      just flow through. Fine, but confirm clients that expect certain response
      framing aren't breaking silently.
- [ ] No graceful shutdown. SIGTERM kills in-flight transfers mid-body.
- [ ] `XFEATURE COMPRESS GZIP` is rejected with 500. If providers ever charge
      less for compressed transfer, this is worth revisiting.
- [ ] No tests. Mock NNTP server + a session integration test would catch most
      regressions cheaply — `nzb-nntp` already has `testutil::MockNntpServer`.

## Near-term — make the proxy closer to production

- [ ] **Idle connection pool.** Return authenticated upstream connections to
      an idle vec when a client disconnects. Reuse them on the next acquire
      (with a liveness check) before opening a new TCP+TLS connection.
      `nzb-nntp::pool::ConnectionPool` does this already — worth revisiting
      whether to expose its raw transport or port the pattern.
- [ ] **Client auth.** Validate `AUTHINFO USER` + `PASS` against *something*.
      For now a static map from env/config is enough (`PROXY_USERS=alice:secret,bob:secret2`).
      Full App Server integration comes later.
- [ ] **TLS to clients.** Listen on 563 with a cert (Let's Encrypt via a reverse
      proxy, or rustls server config directly). Users on the public internet
      shouldn't be sending `AUTHINFO PASS` in plaintext.
- [ ] **Metrics.** Prometheus endpoint on a separate port: pool utilisation,
      active sessions, bytes/sec upstream + downstream, per-client counters.
- [ ] **Graceful shutdown.** Catch SIGTERM, stop accepting, let in-flight
      sessions finish or timeout cleanly.
- [ ] **Per-session byte counting.** Needed for any future usage billing
      anyway; cheap to add now.

## Scaling levers — when user load justifies it

- [ ] **Multi-account outbound.** Support N upstream accounts, round-robin
      across them on acquire. Each account has its own connection cap.
      Doubles/triples capacity without reseller negotiation.
- [ ] **Article cache.** Message-id keyed LRU (disk-backed, maybe Redis or just
      a filesystem cache). Popular releases downloaded once, served N times.
      Biggest single bandwidth multiplier.
- [ ] **Regional proxies + GeoDNS.** Only if Australian/Asian users complain
      about throughput. NNTP's multi-connection nature masks a lot of latency.
- [ ] **Horizontal scaling.** Multiple proxy instances behind a TCP load
      balancer. Requires connection stickiness OR shared session state so
      multi-round-trip NNTP commands don't span instances.

## App Server (separate work stream)

This is the next major component; don't start until the POC proxy is
stable.

- [ ] Choose stack. Likely Rust + Axum (fits the ecosystem). Could reuse
      StackArr's `stackarr-web` as a starting point.
- [ ] User model: signup, login, sessions, API tokens.
- [ ] Per-user proxy credentials: issue random creds, store hash, proxy
      validates against App Server on `AUTHINFO`.
- [ ] Provision `nzb.indexarr` API creds per user (indexarr supports this
      already? Confirm).
- [ ] Usage tracking: proxy reports byte counts + session events to App Server.
- [ ] Admin UI: list users, revoke, view usage.
- [ ] Billing integration (Stripe most likely) — only if/when going paid.

## Client-side / UX

- [ ] Clean up the test client. It's NZBFailTest copied verbatim; at minimum
      the hardcoded fallback server list should be removed.
- [ ] Decide what the **actual** end-user client is. Options:
      - Existing SABnzbd / NZBGet / NZBHydra configured to point at our proxy
      - Rebrand `rustnzbd` as the official client
      - Build a minimal web UI on top of the App Server

## Infra / deployment

- [ ] Pick a deployment target. Probably Hetzner dedicated based on bandwidth
      math — revisit once user projection is clearer.
- [ ] Push images to `repo.indexarr.net` (Forgejo container registry) once
      the proxy is past toy stage. Wire up Forgejo CI.
- [ ] Work out secret management — Infisical is the existing pattern; fits here
      too.
- [ ] Logging / observability — push the `./logs/*.log` files into Loki (Node B
      already has it).

## Open questions / decisions to make

- [ ] **Reseller vs multi-account.** Contact Eweka / Astraweb / XSUsenet about
      reseller arrangements? Or just stack block accounts? Depends on projected
      scale — decide before committing to infra.
- [ ] **Does the proxy also serve as the NZB search front-end?** The diagram
      has them separate (client → nzb.indexarr directly). That's cleaner but
      means two HTTP origins for the client. Alternative: proxy everything
      through a single endpoint. Trade-off between clean separation and fewer
      moving parts for clients.
- [ ] **Cert-based client auth?** Instead of AUTHINFO USER/PASS, issue each
      user a TLS client certificate. More secure, slightly harder for users
      to configure, eliminates "password in NZB client config" problem.
