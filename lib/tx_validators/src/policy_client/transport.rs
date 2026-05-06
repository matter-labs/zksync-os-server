//! HTTP transport for `PolicyClient` calls to the policy service.
//!
//! Two transports are supported: `http://host:port` / `https://host:port`
//! (TCP) and `unix:///path/to.sock` (UDS). Both are built into a single
//! `reqwest::Client`; `PolicyClient` is unaware of the underlying scheme.

use std::path::PathBuf;
use std::time::Duration;

use bytes::Bytes;

/// Errors raised by the transport layer at request time. All of these are
/// treated as fail-closed by `PolicyClient`; the caller never branches on the
/// variant.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
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
    /// HTTP or HTTPS over TCP. Bearer token injected if set.
    Http {
        url: url::Url,
        auth_token: Option<String>,
    },
    /// Unix domain socket. Bearer token injected if set.
    Unix {
        socket_path: PathBuf,
        auth_token: Option<String>,
    },
}

/// A pooled HTTP client plus the base URL to POST against.
/// Built once at startup; cheap to clone (inner `Arc`).
#[derive(Clone, Debug)]
pub(crate) struct Transport {
    client: reqwest::Client,
    base_url: String,
    auth_token: Option<String>,
}

impl Transport {
    pub fn from_config(config: TransportConfig) -> Result<Self, TransportError> {
        match config {
            TransportConfig::Http { url, auth_token } => {
                let base_url = url.to_string().trim_end_matches('/').to_owned();
                let client = reqwest::Client::builder()
                    .build()
                    .map_err(TransportError::Request)?;
                Ok(Self {
                    client,
                    base_url,
                    auth_token,
                })
            }
            TransportConfig::Unix {
                socket_path,
                auth_token,
            } => {
                let client = reqwest::Client::builder()
                    .unix_socket(socket_path)
                    .build()
                    .map_err(TransportError::Request)?;
                Ok(Self {
                    client,
                    base_url: "http://localhost".to_owned(),
                    auth_token,
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
        let mut req = self
            .client
            .post(url)
            .header("content-type", "application/json")
            .header("accept", "application/json")
            .body(body);
        if let Some(token) = &self.auth_token {
            req = req.header("authorization", format!("Bearer {token}"));
        }
        let response = req.send().await.map_err(TransportError::Request)?;
        let status = response.status();
        if !status.is_success() {
            return Err(TransportError::NonSuccessStatus(status));
        }
        response.bytes().await.map_err(TransportError::Request)
    }
}
