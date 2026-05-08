//! HTTP transport for `PolicyClient` calls to the policy service.
//!
//! Two transports are supported: `http://host:port` (TCP) and
//! `unix:///path/to.sock` (UDS). Both are built into a single
//! `reqwest::Client`; `PolicyClient` is unaware of the underlying scheme.

use std::path::PathBuf;
use std::time::Duration;

use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue};
use secrecy::{ExposeSecret, SecretString};
use serde::{Serialize, de::DeserializeOwned};

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
    /// HTTP over TCP. Bearer token is required and always injected.
    Http {
        url: url::Url,
        auth_token: SecretString,
    },
    /// Unix domain socket. Socket-path filesystem permissions are the access
    /// control; bearer token is not applicable and not sent.
    Unix {
        socket_path: PathBuf,
    },
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
            TransportConfig::Http { url, auth_token } => {
                let base_url = url.to_string().trim_end_matches('/').to_owned();
                let mut auth_value =
                    HeaderValue::from_str(&format!("Bearer {}", auth_token.expose_secret()))
                        .expect("auth token must be a valid HTTP header value");
                auth_value.set_sensitive(true);
                let mut headers = Self::base_headers();
                headers.insert(AUTHORIZATION, auth_value);
                let client = reqwest::Client::builder()
                    .default_headers(headers)
                    .build()
                    .map_err(TransportError::Request)?;
                Ok(Self { client, base_url })
            }
            TransportConfig::Unix { socket_path } => {
                let client = reqwest::Client::builder()
                    .default_headers(Self::base_headers())
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

    fn base_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        headers
    }

    pub async fn post_admit<R: Serialize, D: DeserializeOwned>(
        &self,
        request: &R,
    ) -> Result<D, TransportError> {
        self.post("/admit", request).await
    }

    pub async fn post_judge<R: Serialize, D: DeserializeOwned>(
        &self,
        request: &R,
    ) -> Result<D, TransportError> {
        self.post("/judge", request).await
    }

    async fn post<R: Serialize, D: DeserializeOwned>(
        &self,
        path: &str,
        request: &R,
    ) -> Result<D, TransportError> {
        let url = format!("{}{path}", self.base_url);
        let response = self
            .client
            .post(url)
            .json(request)
            .send()
            .await
            .map_err(TransportError::Request)?
            .error_for_status()
            .map_err(|e: reqwest::Error| match e.status() {
                Some(status) => TransportError::NonSuccessStatus(status),
                None => TransportError::Request(e),
            })?;
        response.json::<D>().await.map_err(|e: reqwest::Error| {
            if e.is_decode() {
                TransportError::MalformedResponse
            } else {
                TransportError::Request(e)
            }
        })
    }
}
