//! Upstream connection pool.
//!
//! Manages a semaphore-limited set of TLS connections to the upstream Usenet
//! server. Each acquired `UpstreamConn` holds an OwnedSemaphorePermit so the
//! slot is freed automatically when the conn is dropped.

use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_rustls::TlsConnector;
use tracing::{debug, info, warn};

use crate::config::ProxyConfig;

/// A live, authenticated upstream NNTP connection.
pub struct UpstreamConn {
    /// Buffered reader over the TLS stream. Write via `get_mut()`.
    pub stream: BufReader<tokio_rustls::client::TlsStream<TcpStream>>,
    /// Freed automatically when this struct is dropped.
    _permit: OwnedSemaphorePermit,
}

impl UpstreamConn {
    /// Send a command line to upstream, appending `\r\n`.
    pub async fn send_line(&mut self, line: &str) -> anyhow::Result<()> {
        let w = self.stream.get_mut();
        w.write_all(line.as_bytes()).await?;
        w.write_all(b"\r\n").await?;
        w.flush().await?;
        Ok(())
    }

    /// Read a single response line from upstream.
    pub async fn read_line(&mut self) -> anyhow::Result<String> {
        let mut line = String::new();
        let n = self.stream.read_line(&mut line).await?;
        if n == 0 {
            anyhow::bail!("upstream closed connection");
        }
        Ok(line)
    }

    /// Read a multi-line NNTP body (dot-terminated) and return the raw bytes
    /// including the `.\r\n` terminator, ready to forward to the client.
    pub async fn read_multiline_body(&mut self) -> anyhow::Result<Vec<u8>> {
        let mut body = Vec::new();
        loop {
            let mut line: Vec<u8> = Vec::new();
            let n = self.stream.read_until(b'\n', &mut line).await?;
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
}

/// Semaphore-backed pool — creates a fresh TLS connection per acquire().
pub struct UpstreamPool {
    config: Arc<ProxyConfig>,
    semaphore: Arc<Semaphore>,
    tls_config: Arc<rustls::ClientConfig>,
}

impl UpstreamPool {
    pub fn new(config: Arc<ProxyConfig>) -> Self {
        let tls_config = Arc::new(build_tls_config());
        let semaphore = Arc::new(Semaphore::new(config.max_connections));
        Self {
            config,
            semaphore,
            tls_config,
        }
    }

    /// Acquire a slot and return a fresh authenticated upstream connection.
    /// Blocks (async) while all slots are occupied.
    pub async fn acquire(&self) -> anyhow::Result<UpstreamConn> {
        let permit = Arc::clone(&self.semaphore)
            .acquire_owned()
            .await
            .map_err(|_| anyhow::anyhow!("upstream pool semaphore closed"))?;

        let available = self.semaphore.available_permits();
        debug!(available, max = self.config.max_connections, "pool slot acquired");

        match self.connect().await {
            Ok(stream) => {
                info!("upstream connection ready");
                Ok(UpstreamConn {
                    stream,
                    _permit: permit,
                })
            }
            Err(e) => {
                warn!("upstream connect failed: {e}");
                // permit drops here, freeing the slot
                Err(e)
            }
        }
    }

    pub fn available_permits(&self) -> usize {
        self.semaphore.available_permits()
    }

    async fn connect(
        &self,
    ) -> anyhow::Result<BufReader<tokio_rustls::client::TlsStream<TcpStream>>> {
        let addr = format!("{}:{}", self.config.upstream_host, self.config.upstream_port);
        let tcp = TcpStream::connect(&addr).await?;
        tcp.set_nodelay(true).ok();

        let connector = TlsConnector::from(Arc::clone(&self.tls_config));
        let server_name =
            rustls_pki_types::ServerName::try_from(self.config.upstream_host.clone())
                .map_err(|e| anyhow::anyhow!("invalid upstream hostname: {e}"))?;

        let tls = connector.connect(server_name, tcp).await?;
        let mut stream = BufReader::with_capacity(256 * 1024, tls);

        // Welcome banner
        let mut banner = String::new();
        stream.read_line(&mut banner).await?;
        let code = parse_code(&banner);
        if !matches!(code, 200 | 201) {
            anyhow::bail!("unexpected upstream welcome (code {code}): {}", banner.trim());
        }
        debug!("upstream welcome: {}", banner.trim());

        // AUTHINFO USER
        stream
            .get_mut()
            .write_all(format!("AUTHINFO USER {}\r\n", self.config.upstream_user).as_bytes())
            .await?;
        stream.get_mut().flush().await?;
        let mut resp = String::new();
        stream.read_line(&mut resp).await?;
        debug!("AUTHINFO USER: {}", resp.trim());

        // AUTHINFO PASS
        stream
            .get_mut()
            .write_all(format!("AUTHINFO PASS {}\r\n", self.config.upstream_pass).as_bytes())
            .await?;
        stream.get_mut().flush().await?;
        let mut resp = String::new();
        stream.read_line(&mut resp).await?;
        let code = parse_code(&resp);
        if code != 281 {
            anyhow::bail!(
                "upstream auth failed (code {code}): {}",
                resp.trim()
            );
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
