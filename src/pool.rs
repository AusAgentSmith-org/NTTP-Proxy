//! Upstream connection pool with idle reuse.
//!
//! Lifecycle of a connection:
//!
//!   [none]   ──acquire (no idle available)──►   new TCP+TLS+AUTH   ──►   in use
//!   in use   ──drop without release_clean()──►  close + free permit
//!   in use   ──release_clean()──►              idle (permit retained)
//!   idle     ──acquire──►                       reused, in use
//!   idle     ──TTL sweep──►                     close + free permit
//!
//! The OS-level TCP connection count never exceeds `max_connections`: idle
//! connections carry their permit with them, so the semaphore accounts for
//! active + idle combined. Providers cap by connection count, not activity,
//! so this matches what they see.
//!
//! Sessions mark a connection "clean" only on a clean client QUIT. Any other
//! exit path (error, client disconnect mid-command, upstream error) discards
//! the connection since it may be in an unknown NNTP protocol state.

use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_rustls::TlsConnector;
use tracing::{debug, info, warn};

use crate::config::ProxyConfig;

type TlsStream = tokio_rustls::client::TlsStream<TcpStream>;
type PooledStream = BufReader<TlsStream>;

const IDLE_TTL: Duration = Duration::from_secs(60);

/// A live, authenticated upstream NNTP connection.
///
/// Drop semantics: unless `release_clean()` is called, the connection is
/// closed on drop and the semaphore slot freed. `release_clean()` hands it
/// back to the idle pool for reuse.
pub struct UpstreamConn {
    /// Buffered reader over the TLS stream. Write via `get_mut()`.
    pub stream: Option<PooledStream>,
    permit: Option<OwnedSemaphorePermit>,
    pool: Arc<UpstreamPool>,
}

impl UpstreamConn {
    pub async fn send_line(&mut self, line: &str) -> anyhow::Result<()> {
        let s = self.stream.as_mut().expect("stream present");
        let w = s.get_mut();
        w.write_all(line.as_bytes()).await?;
        w.write_all(b"\r\n").await?;
        w.flush().await?;
        Ok(())
    }

    pub async fn read_line(&mut self) -> anyhow::Result<String> {
        let s = self.stream.as_mut().expect("stream present");
        let mut line = String::new();
        let n = s.read_line(&mut line).await?;
        if n == 0 {
            anyhow::bail!("upstream closed connection");
        }
        Ok(line)
    }

    pub async fn read_multiline_body(&mut self) -> anyhow::Result<Vec<u8>> {
        let s = self.stream.as_mut().expect("stream present");
        let mut body = Vec::new();
        loop {
            let mut line: Vec<u8> = Vec::new();
            let n = s.read_until(b'\n', &mut line).await?;
            if n == 0 {
                anyhow::bail!("upstream closed mid-multiline body");
            }
            let is_terminator = line == b".\r\n" || line == b".\n";
            body.extend_from_slice(&line);
            if is_terminator {
                break;
            }
        }
        Ok(body)
    }

    /// Return this connection to the idle pool for reuse by the next caller.
    /// Only safe after a clean protocol state (e.g. at the end of a client
    /// session that exited via QUIT).
    pub fn release_clean(mut self) {
        let stream = self.stream.take();
        let permit = self.permit.take();
        if let (Some(s), Some(p)) = (stream, permit) {
            self.pool.push_idle(s, p);
        }
        // Drop runs normally; fields are None so the auto-discard is a no-op.
    }
}

impl Drop for UpstreamConn {
    fn drop(&mut self) {
        // Any remaining resources here represent an un-clean exit — let them
        // close. The permit drops, freeing the slot.
        if self.stream.is_some() {
            debug!("upstream connection discarded (not clean)");
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────

struct IdleEntry {
    stream: PooledStream,
    permit: OwnedSemaphorePermit,
    parked_at: Instant,
}

pub struct UpstreamPool {
    config: Arc<ProxyConfig>,
    semaphore: Arc<Semaphore>,
    tls_config: Arc<rustls::ClientConfig>,
    idle: Mutex<Vec<IdleEntry>>,
}

impl UpstreamPool {
    pub fn new(config: Arc<ProxyConfig>) -> Arc<Self> {
        let tls_config = Arc::new(build_tls_config());
        let semaphore = Arc::new(Semaphore::new(config.max_connections));
        let pool = Arc::new(Self {
            config,
            semaphore,
            tls_config,
            idle: Mutex::new(Vec::new()),
        });

        // Start TTL sweeper — drops idle connections older than IDLE_TTL.
        let pool_clone = Arc::clone(&pool);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(10));
            tick.tick().await;
            loop {
                tick.tick().await;
                pool_clone.sweep_idle();
            }
        });

        pool
    }

    /// Acquire a connection — reuse idle if available, otherwise create new.
    /// Blocks (async) if every slot is held by an in-use session.
    pub async fn acquire(self: &Arc<Self>) -> anyhow::Result<UpstreamConn> {
        // Fast path: is there a ready idle conn?
        if let Some(entry) = self.idle.lock().pop() {
            debug!(
                idle_remaining = self.idle.lock().len(),
                "reusing idle upstream connection"
            );
            return Ok(UpstreamConn {
                stream: Some(entry.stream),
                permit: Some(entry.permit),
                pool: Arc::clone(self),
            });
        }

        // Slow path: wait for a permit, then open a fresh TCP+TLS+AUTH.
        let permit = Arc::clone(&self.semaphore)
            .acquire_owned()
            .await
            .map_err(|_| anyhow::anyhow!("upstream pool semaphore closed"))?;

        debug!(
            available = self.semaphore.available_permits(),
            max = self.config.max_connections,
            "new upstream slot acquired"
        );

        match self.connect().await {
            Ok(stream) => {
                info!("upstream connection established");
                Ok(UpstreamConn {
                    stream: Some(stream),
                    permit: Some(permit),
                    pool: Arc::clone(self),
                })
            }
            Err(e) => {
                warn!("upstream connect failed: {e}");
                Err(e)
            }
        }
    }

    pub fn available_permits(&self) -> usize {
        self.semaphore.available_permits()
    }

    #[allow(dead_code)] // exposed for future metrics endpoint
    pub fn idle_count(&self) -> usize {
        self.idle.lock().len()
    }

    /// Called from `UpstreamConn::release_clean`. Keep the stream + its
    /// permit together so the semaphore accounts for idle connections.
    fn push_idle(&self, stream: PooledStream, permit: OwnedSemaphorePermit) {
        let mut idle = self.idle.lock();
        idle.push(IdleEntry {
            stream,
            permit,
            parked_at: Instant::now(),
        });
        debug!(idle_count = idle.len(), "connection parked in idle pool");
    }

    /// Drop idle connections older than IDLE_TTL. Each drop frees a permit.
    fn sweep_idle(&self) {
        let mut idle = self.idle.lock();
        let before = idle.len();
        let now = Instant::now();
        idle.retain(|e| now.duration_since(e.parked_at) < IDLE_TTL);
        let dropped = before - idle.len();
        if dropped > 0 {
            debug!(dropped, remaining = idle.len(), "idle pool sweep");
        }
    }

    async fn connect(&self) -> anyhow::Result<PooledStream> {
        let addr = format!(
            "{}:{}",
            self.config.upstream_host, self.config.upstream_port
        );
        let tcp = TcpStream::connect(&addr).await?;
        tcp.set_nodelay(true).ok();

        let connector = TlsConnector::from(Arc::clone(&self.tls_config));
        let server_name = rustls_pki_types::ServerName::try_from(self.config.upstream_host.clone())
            .map_err(|e| anyhow::anyhow!("invalid upstream hostname: {e}"))?;

        let tls = connector.connect(server_name, tcp).await?;
        let mut stream = BufReader::with_capacity(256 * 1024, tls);

        // Welcome banner
        let mut banner = String::new();
        stream.read_line(&mut banner).await?;
        let code = parse_code(&banner);
        if !matches!(code, 200 | 201) {
            anyhow::bail!(
                "unexpected upstream welcome (code {code}): {}",
                banner.trim()
            );
        }
        debug!("upstream welcome: {}", banner.trim());

        stream
            .get_mut()
            .write_all(format!("AUTHINFO USER {}\r\n", self.config.upstream_user).as_bytes())
            .await?;
        stream.get_mut().flush().await?;
        let mut resp = String::new();
        stream.read_line(&mut resp).await?;
        debug!("AUTHINFO USER: {}", resp.trim());

        stream
            .get_mut()
            .write_all(format!("AUTHINFO PASS {}\r\n", self.config.upstream_pass).as_bytes())
            .await?;
        stream.get_mut().flush().await?;
        let mut resp = String::new();
        stream.read_line(&mut resp).await?;
        let code = parse_code(&resp);
        if code != 281 {
            anyhow::bail!("upstream auth failed (code {code}): {}", resp.trim());
        }
        debug!("upstream authenticated");

        Ok(stream)
    }
}

pub fn parse_code(line: &str) -> u16 {
    line.split_whitespace()
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

fn build_tls_config() -> rustls::ClientConfig {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut root_store = rustls::RootCertStore::empty();
    root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("TLS protocol versions")
        .with_root_certificates(root_store)
        .with_no_client_auth()
}
