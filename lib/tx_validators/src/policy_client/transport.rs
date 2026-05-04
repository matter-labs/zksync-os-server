//! HTTP transport for `PolicyClient` calls to the policy service.
//!
//! Two transports are supported: `https://host:port` (TCP, mTLS) and
//! `unix:///path/to.sock` (UDS, no TLS). Both are built into a single
//! `reqwest::Client`; `PolicyClient` is unaware of the underlying scheme.

use std::io::BufReader;
use std::io::Cursor;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::{ClientConfig, RootCertStore};

use super::TlsConfig;

/// Errors raised by the transport layer at request time. All of these are
/// treated as fail-closed by `PolicyClient`; the caller never branches on the
/// variant.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("invalid TLS config: {0}")]
    TlsConfig(String),
    #[error("request error: {0}")]
    Request(reqwest::Error),
    #[error("non-success status: {0}")]
    NonSuccessStatus(reqwest::StatusCode),
    #[error("timed out after {0:?}")]
    Timeout(Duration),
    #[error("response body is not valid JSON or missing required fields")]
    MalformedResponse,
    #[error("response protocolVersion does not match expected")]
    ProtocolVersionMismatch,
}

/// Type-safe transport configuration; the variant encodes the scheme-specific
/// invariants so that `Transport::from_config` can rely on them without
/// further validation.
pub(crate) enum TransportConfig {
    /// mTLS over TCP. `url` is the validated service URL (path preserved).
    Https { url: url::Url, tls: TlsConfig },
    /// UDS. `socket_path` is the absolute path to the socket file.
    Unix { socket_path: PathBuf },
}

/// A pooled HTTP client plus the base URL to POST against.
/// Built once at startup; cheap to clone (inner `Arc`).
#[derive(Clone, Debug)]
pub(crate) struct Transport {
    client: reqwest::Client,
    base_url: String,
}

impl Transport {
    pub fn from_config(config: TransportConfig) -> Result<Self, TransportError> {
        match config {
            TransportConfig::Https { url, tls } => {
                // Preserve the configured path so the URL can point at a
                // non-root mount (e.g. `https://gateway.internal/policy`).
                let base_url = url.to_string().trim_end_matches('/').to_owned();
                let rustls_config = build_client_config(&tls)?;
                let client = reqwest::Client::builder()
                    .use_preconfigured_tls(rustls_config)
                    .https_only(true)
                    .build()
                    .map_err(TransportError::Request)?;
                Ok(Self { client, base_url })
            }
            TransportConfig::Unix { socket_path } => {
                let client = reqwest::Client::builder()
                    .unix_socket(socket_path)
                    .build()
                    .map_err(TransportError::Request)?;
                Ok(Self {
                    client,
                    base_url: "http://localhost".to_owned(),
                })
            }
        }
    }

    pub async fn post_admit(&self, body: Vec<u8>) -> Result<Bytes, TransportError> {
        self.post("/admit", body).await
    }

    pub async fn post_judge(&self, body: Vec<u8>) -> Result<Bytes, TransportError> {
        self.post("/judge", body).await
    }

    async fn post(&self, path: &str, body: Vec<u8>) -> Result<Bytes, TransportError> {
        let url = format!("{}{path}", self.base_url);
        let response = self
            .client
            .post(url)
            .header("content-type", "application/json")
            .header("accept", "application/json")
            .body(body)
            .send()
            .await
            .map_err(TransportError::Request)?;
        let status = response.status();
        if !status.is_success() {
            return Err(TransportError::NonSuccessStatus(status));
        }
        response.bytes().await.map_err(TransportError::Request)
    }
}

fn build_client_config(tls: &TlsConfig) -> Result<ClientConfig, TransportError> {
    let mut roots = RootCertStore::empty();
    let server_ca_certs = read_certs(&tls.server_ca)?;
    if server_ca_certs.is_empty() {
        return Err(TransportError::TlsConfig(
            "no certificates found in server_ca".into(),
        ));
    }
    for cert in server_ca_certs {
        roots
            .add(cert)
            .map_err(|err| TransportError::TlsConfig(format!("invalid CA cert: {err}")))?;
    }

    let client_certs = read_certs(&tls.client_cert)?;
    if client_certs.is_empty() {
        return Err(TransportError::TlsConfig(
            "no certificates found in client_cert".into(),
        ));
    }
    let client_key = read_private_key(&tls.client_key)?;

    // Use the ring provider explicitly rather than relying on a process-wide
    // default — `PolicyClient` is library code and shouldn't depend on
    // whoever (if anyone) installed a default provider at startup.
    let provider = rustls::crypto::ring::default_provider();
    let config = ClientConfig::builder_with_provider(Arc::new(provider))
        .with_safe_default_protocol_versions()
        .map_err(|err| TransportError::TlsConfig(err.to_string()))?
        .with_root_certificates(roots)
        .with_client_auth_cert(client_certs, client_key)
        .map_err(|err| TransportError::TlsConfig(err.to_string()))?;

    Ok(config)
}

fn read_certs(pem: &str) -> Result<Vec<CertificateDer<'static>>, TransportError> {
    let mut reader = BufReader::new(Cursor::new(pem.as_bytes()));
    rustls_pemfile::certs(&mut reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| TransportError::TlsConfig(format!("failed to parse cert PEM: {err}")))
}

fn read_private_key(pem: &str) -> Result<PrivateKeyDer<'static>, TransportError> {
    let mut reader = BufReader::new(Cursor::new(pem.as_bytes()));
    rustls_pemfile::private_key(&mut reader)
        .map_err(|err| TransportError::TlsConfig(format!("failed to parse key PEM: {err}")))?
        .ok_or_else(|| TransportError::TlsConfig("no private key found in client_key".into()))
}
