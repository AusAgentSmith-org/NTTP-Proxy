//! Per-client NNTP session.
//!
//! Lifecycle:
//!   1. Send welcome banner
//!   2. Auth phase: AUTHINFO USER caches the username, AUTHINFO PASS validates
//!      the (user, pass) pair via the app-server (if configured), enforces the
//!      per-user max_connections cap, then acquires an upstream connection.
//!   3. Pass-through phase: forward commands → upstream, forward responses →
//!      client. Multi-line response bodies are forwarded byte-for-byte.
//!   4. If the user gets locked, a cancel signal trips and the session ends.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tracing::{debug, info, warn};

use crate::app_client::AppClient;
use crate::config::ProxyConfig;
use crate::pool::{UpstreamConn, UpstreamPool, parse_code};
use crate::user_pool::{SessionGuard, UserPool};

/// Default per-user cap when running without an app-server (open mode).
const OPEN_MODE_CAP: u32 = 32;

pub async fn handle(
    socket: TcpStream,
    peer: SocketAddr,
    _cfg: Arc<ProxyConfig>,
    pool: Arc<UpstreamPool>,
    user_pool: Arc<UserPool>,
    app: Option<AppClient>,
) -> anyhow::Result<()> {
    let mut client = BufReader::new(socket);

    write_client(&mut client, b"200 nntp-proxy ready - posting ok\r\n").await?;

    let mut user_buf: Option<String> = None;
    let mut upstream: Option<UpstreamConn> = None;
    // _user_guard: kept only for its Drop impl (releases per-user semaphore permit
    // and removes the session from the registry).
    let mut _user_guard: Option<SessionGuard> = None;
    let mut cancel_rx: Option<oneshot::Receiver<()>> = None;
    let mut auth_user: Option<String> = None;

    loop {
        // Race the next client line against an optional cancel signal.
        let line_fut = read_line_owned(&mut client);
        let next = if let Some(rx) = cancel_rx.as_mut() {
            tokio::select! {
                _ = rx => {
                    warn!(%peer, user = ?auth_user, "session cancelled (user locked)");
                    let _ = write_client(&mut client, b"482 Account locked - disconnecting\r\n").await;
                    return Ok(());
                }
                r = line_fut => r,
            }
        } else {
            line_fut.await
        };

        let line = match next {
            Ok(Some(s)) => s,
            Ok(None) => {
                debug!(%peer, "client disconnected");
                return Ok(());
            }
            Err(e) => {
                debug!(%peer, "client read error: {e}");
                return Ok(());
            }
        };

        let trimmed = line.trim_end_matches(['\r', '\n']).trim();
        if trimmed.is_empty() {
            continue;
        }
        debug!(%peer, cmd = %trimmed, "←");

        let upper = trimmed.to_ascii_uppercase();

        // ── QUIT ────────────────────────────────────────────────────────────
        if upper.starts_with("QUIT") {
            write_client(&mut client, b"205 closing connection - goodbye!\r\n").await?;
            info!(%peer, user = ?auth_user, "session closed by client QUIT");
            return Ok(());
        }

        // ── AUTHINFO USER ───────────────────────────────────────────────────
        if upper.starts_with("AUTHINFO USER") {
            let username = trimmed
                .splitn(3, char::is_whitespace)
                .nth(2)
                .unwrap_or("")
                .trim()
                .to_string();
            user_buf = Some(username);
            write_client(&mut client, b"381 Password required\r\n").await?;
            continue;
        }

        // ── AUTHINFO PASS ───────────────────────────────────────────────────
        if upper.starts_with("AUTHINFO PASS") {
            if upstream.is_some() {
                write_client(&mut client, b"281 Authentication accepted\r\n").await?;
                continue;
            }

            let password = trimmed.splitn(3, char::is_whitespace).nth(2).unwrap_or("");
            let username = user_buf.clone().unwrap_or_default();

            // 1. Validate credentials + get per-user cap (or fall back if no app-server).
            let cap = match &app {
                Some(client_) => match client_.validate(&username, password).await {
                    Ok(r) if r.allowed => r.max_connections,
                    Ok(r) => {
                        warn!(%peer, user = %username, reason = ?r.reason, "AUTH rejected by app-server");
                        write_client(&mut client, b"481 Authentication failed\r\n").await?;
                        continue;
                    }
                    Err(e) => {
                        warn!(%peer, user = %username, "app-server validate error: {e}");
                        write_client(&mut client, b"481 Authentication unavailable\r\n").await?;
                        continue;
                    }
                },
                None => OPEN_MODE_CAP,
            };

            // 2. Acquire per-user permit (waits if user is at their cap).
            let (guard, rx) = match user_pool.acquire(&username, cap).await {
                Ok(p) => p,
                Err(e) => {
                    warn!(%peer, user = %username, "user pool acquire failed: {e}");
                    write_client(&mut client, b"481 Authentication failed\r\n").await?;
                    continue;
                }
            };
            _user_guard = Some(guard);
            cancel_rx = Some(rx);
            auth_user = Some(username.clone());

            // 3. Acquire upstream provider connection.
            info!(%peer, user = %username, cap, available = pool.available_permits(),
                  "acquiring upstream slot");
            match pool.acquire().await {
                Ok(conn) => {
                    upstream = Some(conn);
                    write_client(&mut client, b"281 Authentication accepted\r\n").await?;
                    info!(%peer, user = %username, "authenticated, upstream acquired");
                }
                Err(e) => {
                    warn!(%peer, "upstream acquire failed: {e}");
                    write_client(&mut client, b"481 upstream unavailable\r\n").await?;
                    _user_guard = None;
                    cancel_rx = None;
                    auth_user = None;
                }
            }
            continue;
        }

        // ── XFEATURE COMPRESS — reject; we can't proxy compressed upstream ──
        if upper.starts_with("XFEATURE COMPRESS") {
            write_client(&mut client, b"500 not supported\r\n").await?;
            continue;
        }

        // ── All other commands: need upstream ────────────────────────────────
        if upstream.is_none() {
            // Open mode (no app-server) lets clients skip AUTHINFO entirely.
            // With app-server enabled we require AUTH before any real command.
            if app.is_some() {
                write_client(&mut client, b"480 Authentication required\r\n").await?;
                continue;
            }
            info!(%peer, "no upstream yet — acquiring on first command (open mode)");
            // Synthetic anonymous user so per-user accounting still works.
            let username = "__anon__".to_string();
            let (guard, rx) = match user_pool.acquire(&username, OPEN_MODE_CAP).await {
                Ok(p) => p,
                Err(e) => {
                    warn!(%peer, "user pool acquire failed: {e}");
                    write_client(&mut client, b"400 Service temporarily unavailable\r\n").await?;
                    return Ok(());
                }
            };
            _user_guard = Some(guard);
            cancel_rx = Some(rx);
            auth_user = Some(username);
            match pool.acquire().await {
                Ok(conn) => upstream = Some(conn),
                Err(e) => {
                    warn!(%peer, "upstream acquire failed: {e}");
                    write_client(&mut client, b"400 Service temporarily unavailable\r\n").await?;
                    return Ok(());
                }
            }
        }

        let up = upstream.as_mut().unwrap();
        let user = auth_user.as_deref().unwrap_or("");

        // Forward command to upstream
        if let Err(e) = up.send_line(trimmed).await {
            warn!(%peer, "upstream write error: {e}");
            write_client(&mut client, b"400 upstream connection lost\r\n").await?;
            return Ok(());
        }

        // Read upstream response status line
        let resp = match up.read_line().await {
            Ok(l) => l,
            Err(e) => {
                warn!(%peer, "upstream read error: {e}");
                write_client(&mut client, b"400 upstream connection lost\r\n").await?;
                return Ok(());
            }
        };

        let code = parse_code(&resp);
        debug!(%peer, code, resp = %resp.trim(), "→");

        // Forward status line to client
        write_client(&mut client, resp.as_bytes()).await?;
        user_pool.record_bytes(user, resp.len() as u64);

        // Forward multi-line body if applicable
        if is_multiline(code) {
            match up.read_multiline_body().await {
                Ok(body) => {
                    let n = body.len() as u64;
                    if let Err(e) = write_client(&mut client, &body).await {
                        warn!(%peer, "client write error during body: {e}");
                        return Ok(());
                    }
                    user_pool.record_bytes(user, n);
                }
                Err(e) => {
                    warn!(%peer, "upstream multiline read error: {e}");
                    return Ok(());
                }
            }
        }
    }
}

async fn read_line_owned(
    client: &mut BufReader<TcpStream>,
) -> std::io::Result<Option<String>> {
    let mut line = String::new();
    let n = client.read_line(&mut line).await?;
    if n == 0 { Ok(None) } else { Ok(Some(line)) }
}

async fn write_client(client: &mut BufReader<TcpStream>, data: &[u8]) -> anyhow::Result<()> {
    client.get_mut().write_all(data).await?;
    client.get_mut().flush().await?;
    Ok(())
}

/// Whether an NNTP response code is followed by a dot-terminated multi-line body.
fn is_multiline(code: u16) -> bool {
    matches!(
        code,
        215 | 220 | 221 | 222 | 224 | 225 | 230 | 231 | 282
    )
}
