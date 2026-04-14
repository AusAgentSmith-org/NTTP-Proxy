//! Per-client NNTP session.
//!
//! Lifecycle:
//!   1. Send welcome banner
//!   2. Auth phase: intercept AUTHINFO USER/PASS, validate, acquire upstream slot
//!   3. Pass-through phase: forward commands → upstream, forward responses → client
//!      Multi-line response bodies (BODY, ARTICLE, HEAD, XOVER, …) are forwarded
//!      byte-for-byte including the `.\r\n` terminator.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tracing::{debug, info, warn};

use crate::config::ProxyConfig;
use crate::pool::{UpstreamConn, UpstreamPool, parse_code};

pub async fn handle(
    socket: TcpStream,
    peer: SocketAddr,
    _cfg: Arc<ProxyConfig>,
    pool: Arc<UpstreamPool>,
) -> anyhow::Result<()> {
    let mut client = BufReader::new(socket);

    write_client(&mut client, b"200 nntp-proxy ready - posting ok\r\n").await?;

    let mut upstream: Option<UpstreamConn> = None;

    loop {
        let mut line = String::new();
        let n = client.read_line(&mut line).await?;
        if n == 0 {
            debug!(%peer, "client disconnected");
            return Ok(());
        }

        let trimmed = line.trim_end_matches(['\r', '\n']).trim();
        if trimmed.is_empty() {
            continue;
        }
        debug!(%peer, cmd = %trimmed, "←");

        let upper = trimmed.to_ascii_uppercase();

        // ── QUIT ────────────────────────────────────────────────────────────
        if upper.starts_with("QUIT") {
            write_client(&mut client, b"205 closing connection - goodbye!\r\n").await?;
            info!(%peer, "session closed by client QUIT");
            return Ok(());
        }

        // ── AUTHINFO USER ───────────────────────────────────────────────────
        if upper.starts_with("AUTHINFO USER") {
            write_client(&mut client, b"381 Password required\r\n").await?;
            continue;
        }

        // ── AUTHINFO PASS ───────────────────────────────────────────────────
        if upper.starts_with("AUTHINFO PASS") {
            if upstream.is_some() {
                // Already authenticated — re-auth not needed
                write_client(&mut client, b"281 Authentication accepted\r\n").await?;
                continue;
            }
            info!(%peer, available = pool.available_permits(), "acquiring upstream slot");
            match pool.acquire().await {
                Ok(conn) => {
                    upstream = Some(conn);
                    write_client(&mut client, b"281 Authentication accepted\r\n").await?;
                    info!(%peer, "authenticated, upstream acquired");
                }
                Err(e) => {
                    warn!(%peer, "upstream acquire failed: {e}");
                    write_client(&mut client, b"481 Authentication failed\r\n").await?;
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
            // Auto-connect for clients that skip AUTHINFO (e.g. plain-text test)
            info!(%peer, "no upstream yet — acquiring on first command");
            match pool.acquire().await {
                Ok(conn) => {
                    upstream = Some(conn);
                }
                Err(e) => {
                    warn!(%peer, "upstream acquire failed: {e}");
                    write_client(&mut client, b"400 Service temporarily unavailable\r\n").await?;
                    return Ok(());
                }
            }
        }

        let up = upstream.as_mut().unwrap();

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

        // Forward multi-line body if applicable
        if is_multiline(code) {
            match up.read_multiline_body().await {
                Ok(body) => {
                    if let Err(e) = write_client(&mut client, &body).await {
                        warn!(%peer, "client write error during body: {e}");
                        return Ok(());
                    }
                }
                Err(e) => {
                    warn!(%peer, "upstream multiline read error: {e}");
                    return Ok(());
                }
            }
        }
    }
}

async fn write_client(client: &mut BufReader<TcpStream>, data: &[u8]) -> anyhow::Result<()> {
    client.get_mut().write_all(data).await?;
    client.get_mut().flush().await?;
    Ok(())
}

/// Whether an NNTP response code is followed by a dot-terminated multi-line body.
/// RFC 3977 §3.1 + RFC 2980 extensions.
fn is_multiline(code: u16) -> bool {
    matches!(
        code,
        215  // LIST / LIST ACTIVE
        | 220  // ARTICLE
        | 221  // HEAD
        | 222  // BODY
        | 224  // OVER / XOVER
        | 225  // HDR (XHDR)
        | 230  // NEWNEWS
        | 231  // NEWGROUPS
        | 282  // XHDR / XPAT data
    )
}
