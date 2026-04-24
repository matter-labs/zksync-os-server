//! HTTP transport for `PolicyClient` calls to the policy service.
//!
//! The policy service supports two wire transports interchangeably:
//! `http://host:port` (TCP) and `unix:///path/to.sock` (UDS). The choice is
//! made at construction time from the URL scheme — `PolicyClient` itself is
//! transport-agnostic. HTTP-over-UDS is the first latency-tuning fallback
//! if TCP round-trips don't hold the latency budget.
//!
//! Both the admit and judge endpoints share the same transport surface;
//! they differ only in the request path passed to [`Transport::post`].

use std::path::{Path, PathBuf};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::{TokioExecutor, TokioIo};
use secrecy::{ExposeSecret, SecretString};
use tokio::net::UnixStream;

/// Errors raised by the transport layer. All of these are treated as fail-closed
/// by `PolicyClient` — the caller never needs to branch on the variant.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("invalid URL: {0}")]
    InvalidUrl(String),
    #[error("unsupported URL scheme `{0}` (expected `http`, `https`, or `unix`)")]
    UnsupportedScheme(String),
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
/// of whether it's talking to TCP or UDS.
#[derive(Clone)]
pub(crate) enum Transport {
    // Boxed so the TCP variant doesn't dominate the enum size — the `Client`
    // pool alone is ~300 bytes, vs <40 bytes for the UDS variant.
    Http(Box<HttpTransport>),
    Unix {
        socket_path: PathBuf,
        auth_token: Option<SecretString>,
    },
}

#[derive(Clone)]
pub(crate) struct HttpTransport {
    pub client: Client<HttpConnector, Full<Bytes>>,
    pub base_url: String,
    pub auth_token: Option<SecretString>,
}

impl Transport {
    pub fn from_url(url: &str, auth_token: Option<SecretString>) -> Result<Self, TransportError> {
        let parsed = url::Url::parse(url).map_err(|e| TransportError::InvalidUrl(e.to_string()))?;
        match parsed.scheme() {
            "http" | "https" => {
                // `base_url` is scheme://host[:port] — strip any path; we append `/admit`
                // ourselves so that the URL env var can be the service root.
                let host = parsed
                    .host_str()
                    .ok_or_else(|| TransportError::InvalidUrl("missing host".into()))?;
                let base_url = match parsed.port() {
                    Some(port) => format!("{}://{host}:{port}", parsed.scheme()),
                    None => format!("{}://{host}", parsed.scheme()),
                };
                let client = Client::builder(TokioExecutor::new())
                    .build::<_, Full<Bytes>>(HttpConnector::new());
                Ok(Self::Http(Box::new(HttpTransport {
                    client,
                    base_url,
                    auth_token,
                })))
            }
            "unix" => {
                let socket_path = parsed.path();
                if socket_path.is_empty() {
                    return Err(TransportError::InvalidUrl(
                        "unix URL missing socket path".into(),
                    ));
                }
                Ok(Self::Unix {
                    socket_path: PathBuf::from(socket_path),
                    auth_token,
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
            Self::Http(http) => {
                let uri = format!("{}{path}", http.base_url);
                let request = build_request(&uri, body, http.auth_token.as_ref())?;
                let response = http
                    .client
                    .request(request)
                    .await
                    .map_err(TransportError::HttpClient)?;
                collect_success_body(response).await
            }
            Self::Unix {
                socket_path,
                auth_token,
            } => post_over_uds(socket_path, path, body, auth_token.as_ref()).await,
        }
    }
}

fn build_request(
    uri: &str,
    body: Vec<u8>,
    auth_token: Option<&SecretString>,
) -> Result<Request<Full<Bytes>>, TransportError> {
    let mut builder = Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header(hyper::header::CONTENT_TYPE, "application/json")
        .header(hyper::header::ACCEPT, "application/json");
    if let Some(token) = auth_token {
        builder = builder.header(
            hyper::header::AUTHORIZATION,
            format!("Bearer {}", token.expose_secret()),
        );
    }
    Ok(builder.body(Full::new(Bytes::from(body)))?)
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
    auth_token: Option<&SecretString>,
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
        .header(hyper::header::ACCEPT, "application/json");
    let request = match auth_token {
        Some(token) => request.header(
            hyper::header::AUTHORIZATION,
            format!("Bearer {}", token.expose_secret()),
        ),
        None => request,
    };
    let request = request.body(Full::new(Bytes::from(body)))?;

    let response = sender
        .send_request(request)
        .await
        .map_err(TransportError::Hyper)?;
    collect_success_body(response).await
}
