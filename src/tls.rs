//! Self-signed cert generation + loading for the NNTPS listener.
//!
//! The proxy generates a long-lived self-signed cert on first startup and
//! persists it to `<TLS_DIR>/cert.pem` + `<TLS_DIR>/key.pem`. Subsequent
//! restarts reuse the same cert, so its SHA-256 fingerprint is stable —
//! which is the thing a bundled client pins.
//!
//! The cert has no meaningful CN or SAN: we're not validating hostnames,
//! only fingerprints. The app-server exposes the fingerprint via
//! `GET /api/fingerprint` so the client binary can fetch-once-and-pin.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use sha2::{Digest, Sha256};
use tracing::info;

pub struct ServerTls {
    pub config: Arc<ServerConfig>,
    /// Hex-encoded SHA-256 of the DER-encoded end-entity cert.
    pub fingerprint: String,
}

/// Load cert + key from `tls_dir`, generating them if absent.
pub fn load_or_generate(tls_dir: &Path) -> anyhow::Result<ServerTls> {
    let cert_path = tls_dir.join("cert.pem");
    let key_path = tls_dir.join("key.pem");

    if !cert_path.exists() || !key_path.exists() {
        std::fs::create_dir_all(tls_dir)
            .with_context(|| format!("create tls dir {}", tls_dir.display()))?;
        generate_to_disk(&cert_path, &key_path)?;
    }

    let (cert_chain, key) = read_pair(&cert_path, &key_path)?;

    let der = cert_chain
        .first()
        .ok_or_else(|| anyhow::anyhow!("cert.pem contained no certificates"))?;
    let fingerprint = hex::encode(Sha256::digest(der.as_ref()));

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("TLS protocol versions")?
        .with_no_client_auth()
        .with_single_cert(cert_chain, key)
        .context("rustls ServerConfig")?;

    info!(
        cert = %cert_path.display(),
        fingerprint = %fingerprint,
        "NNTPS cert loaded"
    );

    Ok(ServerTls {
        config: Arc::new(config),
        fingerprint,
    })
}

fn generate_to_disk(cert_path: &PathBuf, key_path: &PathBuf) -> anyhow::Result<()> {
    info!("generating self-signed NNTPS cert at {}", cert_path.display());
    // rcgen::generate_simple_self_signed does NotBefore=now, NotAfter=now+10y.
    // The SAN list must be non-empty but can be anything; clients won't verify it.
    let cert =
        rcgen::generate_simple_self_signed(vec!["nntp-proxy".into()]).context("rcgen generate")?;
    std::fs::write(cert_path, cert.cert.pem())
        .with_context(|| format!("write {}", cert_path.display()))?;
    std::fs::write(key_path, cert.key_pair.serialize_pem())
        .with_context(|| format!("write {}", key_path.display()))?;
    Ok(())
}

fn read_pair(
    cert_path: &PathBuf,
    key_path: &PathBuf,
) -> anyhow::Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
    let cert_bytes =
        std::fs::read(cert_path).with_context(|| format!("read {}", cert_path.display()))?;
    let mut rdr = &cert_bytes[..];
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut rdr)
        .filter_map(Result::ok)
        .collect();

    let key_bytes =
        std::fs::read(key_path).with_context(|| format!("read {}", key_path.display()))?;
    let mut rdr = &key_bytes[..];
    let key = rustls_pemfile::private_key(&mut rdr)
        .context("parse private key")?
        .ok_or_else(|| anyhow::anyhow!("no private key found in {}", key_path.display()))?;

    Ok((certs, key))
}
