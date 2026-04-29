//! HTTP transport for `PolicyClient` calls to the policy service.
//!
//! The policy service supports two wire transports interchangeably:
//! `https://host:port` (TCP, mTLS) and `unix:///path/to.sock` (UDS, no TLS —
//! kernel-enforced socket permissions are the auth model). Plain `http://`
//! is rejected. The choice is made at construction time from the URL scheme;
//! `PolicyClient` itself is transport-agnostic.
//!
//! For the HTTPS variant, the rustls `ClientConfig` is built from a CA bundle
//! that pins exactly the policy server's CA — system trust roots are
//! deliberately not loaded. Combined with the client certificate presented
//! during the handshake, this gives mutual authentication.
//!
//! Both the admit and judge endpoints share the same transport surface;
//! they differ only in the request path passed to [`Transport::post`].

use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::{Method, Request, Response, StatusCode};
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::{TokioExecutor, TokioIo};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::{ClientConfig, RootCertStore};
use tokio::net::UnixStream;

use super::TlsConfig;

/// Errors raised by the transport layer. All of these are treated as fail-closed
/// by `PolicyClient` — the caller never needs to branch on the variant.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("invalid URL: {0}")]
    InvalidUrl(String),
    #[error("unsupported URL scheme `{0}` (expected `https` or `unix`)")]
    UnsupportedScheme(String),
    #[error("missing TLS config: https URLs require client_cert/client_key/server_ca")]
    MissingTls,
    #[error("TLS config is set but URL scheme is not `https`")]
    UnexpectedTls,
    #[error("failed to read TLS material at {path}: {source}")]
    TlsRead {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse TLS material at {path}: {reason}")]
    TlsParse { path: PathBuf, reason: String },
    #[error("invalid TLS config: {0}")]
    TlsConfig(String),
    #[error("failed to build request: {0}")]
    BuildRequest(#[from] hyper::http::Error),
    #[error("connection failed: {0}")]
    Connect(std::io::Error),
    #[error("http client error: {0}")]
    HttpClient(hyper_util::client::legacy::Error),
    #[error("hyper error: {0}")]
    Hyper(#[from] hyper::Error),
    #[error("non-success status: {0}")]
    NonSuccessStatus(StatusCode),
    #[error("timed out after {0:?}")]
    Timeout(Duration),
}

/// Selected at construction based on the URL scheme; this is the one seam
/// the task asks us to keep isolated. The rest of `PolicyClient` is unaware
/// of whether it's talking to TLS-over-TCP or UDS.
#[derive(Clone)]
pub(crate) enum Transport {
    // Boxed so the TLS variant doesn't dominate the enum size — the `Client`
    // pool plus rustls config alone is several hundred bytes, vs ~16 bytes
    // for the UDS variant.
    Https(Box<HttpsTransport>),
    Unix { socket_path: PathBuf },
}

#[derive(Clone)]
pub(crate) struct HttpsTransport {
    pub client: Client<HttpsConnector<HttpConnector>, Full<Bytes>>,
    pub base_url: String,
}

impl Transport {
    pub fn from_url(url: &str, tls: Option<&TlsConfig>) -> Result<Self, TransportError> {
        let parsed = url::Url::parse(url).map_err(|e| TransportError::InvalidUrl(e.to_string()))?;
        match parsed.scheme() {
            "https" => {
                let tls = tls.ok_or(TransportError::MissingTls)?;
                // `base_url` is scheme://host[:port] — strip any path; we append `/admit`
                // ourselves so that the URL env var can be the service root.
                let host = parsed
                    .host_str()
                    .ok_or_else(|| TransportError::InvalidUrl("missing host".into()))?;
                let base_url = match parsed.port() {
                    Some(port) => format!("https://{host}:{port}"),
                    None => format!("https://{host}"),
                };
                let client_config = build_client_config(tls)?;
                let https_connector = hyper_rustls::HttpsConnectorBuilder::new()
                    .with_tls_config(client_config)
                    .https_only()
                    .enable_http1()
                    .build();
                let client = Client::builder(TokioExecutor::new()).build(https_connector);
                Ok(Self::Https(Box::new(HttpsTransport { client, base_url })))
            }
            "unix" => {
                if tls.is_some() {
                    return Err(TransportError::UnexpectedTls);
                }
                let socket_path = parsed.path();
                if socket_path.is_empty() {
                    return Err(TransportError::InvalidUrl(
                        "unix URL missing socket path".into(),
                    ));
                }
                Ok(Self::Unix {
                    socket_path: PathBuf::from(socket_path),
                })
            }
            other => Err(TransportError::UnsupportedScheme(other.to_string())),
        }
    }

    pub async fn post_admit(&self, body: Vec<u8>) -> Result<Bytes, TransportError> {
        self.post("/admit", body).await
    }

    pub async fn post_judge(&self, body: Vec<u8>) -> Result<Bytes, TransportError> {
        self.post("/judge", body).await
    }

    async fn post(&self, path: &str, body: Vec<u8>) -> Result<Bytes, TransportError> {
        match self {
            Self::Https(https) => {
                let uri = format!("{}{path}", https.base_url);
                let request = build_request(&uri, body)?;
                let response = https
                    .client
                    .request(request)
                    .await
                    .map_err(TransportError::HttpClient)?;
                collect_success_body(response).await
            }
            Self::Unix { socket_path } => post_over_uds(socket_path, path, body).await,
        }
    }
}

fn build_request(uri: &str, body: Vec<u8>) -> Result<Request<Full<Bytes>>, TransportError> {
    let request = Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header(hyper::header::CONTENT_TYPE, "application/json")
        .header(hyper::header::ACCEPT, "application/json")
        .body(Full::new(Bytes::from(body)))?;
    Ok(request)
}

async fn collect_success_body(response: Response<Incoming>) -> Result<Bytes, TransportError> {
    let status = response.status();
    if !status.is_success() {
        return Err(TransportError::NonSuccessStatus(status));
    }
    let collected = response.into_body().collect().await?;
    Ok(collected.to_bytes())
}

/// UDS path uses a one-shot connection per request. Simpler than plumbing
/// a full pooled hyper-util client with a custom connector.
/// TODO: add pooling alongside the latency-measurement work.
async fn post_over_uds(
    socket_path: &Path,
    path: &str,
    body: Vec<u8>,
) -> Result<Bytes, TransportError> {
    let stream = UnixStream::connect(socket_path)
        .await
        .map_err(TransportError::Connect)?;
    let (mut sender, connection) =
        hyper::client::conn::http1::handshake::<_, Full<Bytes>>(TokioIo::new(stream))
            .await
            .map_err(TransportError::Hyper)?;

    // `http1::handshake` returns a future that must be driven to make progress
    // on the connection — spawn it onto the runtime and let it resolve when
    // `sender` is dropped.
    tokio::spawn(async move {
        if let Err(err) = connection.await {
            tracing::debug!(?err, "policy-client UDS connection finished with error");
        }
    });

    // Host header is irrelevant over UDS but required by HTTP/1.1.
    let request = Request::builder()
        .method(Method::POST)
        .uri(path)
        .header(hyper::header::HOST, "localhost")
        .header(hyper::header::CONTENT_TYPE, "application/json")
        .header(hyper::header::ACCEPT, "application/json")
        .body(Full::new(Bytes::from(body)))?;

    let response = sender
        .send_request(request)
        .await
        .map_err(TransportError::Hyper)?;
    collect_success_body(response).await
}

fn build_client_config(tls: &TlsConfig) -> Result<ClientConfig, TransportError> {
    let mut roots = RootCertStore::empty();
    let server_ca_certs = read_certs(&tls.server_ca)?;
    if server_ca_certs.is_empty() {
        return Err(TransportError::TlsConfig(format!(
            "no certificates found in server_ca {}",
            tls.server_ca.display()
        )));
    }
    for cert in server_ca_certs {
        roots
            .add(cert)
            .map_err(|err| TransportError::TlsConfig(format!("invalid CA cert: {err}")))?;
    }

    let client_certs = read_certs(&tls.client_cert)?;
    if client_certs.is_empty() {
        return Err(TransportError::TlsConfig(format!(
            "no certificates found in client_cert {}",
            tls.client_cert.display()
        )));
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

fn read_certs(path: &Path) -> Result<Vec<CertificateDer<'static>>, TransportError> {
    let file = File::open(path).map_err(|err| TransportError::TlsRead {
        path: path.to_path_buf(),
        source: err,
    })?;
    let mut reader = BufReader::new(file);
    rustls_pemfile::certs(&mut reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| TransportError::TlsParse {
            path: path.to_path_buf(),
            reason: err.to_string(),
        })
}

fn read_private_key(path: &Path) -> Result<PrivateKeyDer<'static>, TransportError> {
    let file = File::open(path).map_err(|err| TransportError::TlsRead {
        path: path.to_path_buf(),
        source: err,
    })?;
    let mut reader = BufReader::new(file);
    rustls_pemfile::private_key(&mut reader)
        .map_err(|err| TransportError::TlsParse {
            path: path.to_path_buf(),
            reason: err.to_string(),
        })?
        .ok_or_else(|| {
            TransportError::TlsConfig(format!("no private key found in {}", path.display()))
        })
}
