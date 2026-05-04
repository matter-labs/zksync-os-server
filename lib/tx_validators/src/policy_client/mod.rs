//! `TxValidator` that delegates each transaction decision to an external
//! HTTP policy service. Any transport, protocol-version, or response
//! parsing error is fail-closed.

mod metrics;
mod tracer;
mod transport;
mod wire;

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use alloy::primitives::Address;
use serde::Serialize;
use zksync_os_interface::error::InvalidTransaction;
use zksync_os_interface::tracing::{
    AnyTxValidator, BeginTxContext, TxValidationResult, TxValidator,
};

use self::metrics::{ErrorReason, Outcome, POLICY_CLIENT_METRICS};
use self::tracer::TraceSlot;
pub use self::tracer::{CallKind, CapturedFrame, Tracer};
use self::transport::{Transport, TransportConfig, TransportError};
use self::wire::{AdmitRequest, JudgeRequest, PolicyResponse};

/// Caller intent forwarded with each request. `Read` is for read-only
/// simulations (`eth_call`); `Write` is for everything else, including
/// `eth_estimateGas` (gas is state-dependent and would otherwise leak
/// via the estimator).
#[derive(Copy, Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AccessType {
    Read,
    Write,
}

#[derive(Clone, Debug)]
pub struct Config {
    /// `https://host:port` (mTLS required) or `unix:///path/to.sock`.
    /// Plain `http://` is rejected at construction.
    pub url: String,
    pub request_timeout: Duration,
    pub protocol_version: String,
    /// If set, responses whose `protocolVersion` is not exactly equal are
    /// rejected.
    pub expected_protocol_version: Option<String>,
    /// Source addresses whose txs skip both calls. Intended for
    /// protocol-internal senders (bootloader, force-deployer) the chain
    /// cannot let an external service refuse without bricking startup.
    pub bypass_from: HashSet<Address>,
    /// mTLS material. Required for `https://`; silently ignored for `unix:///`.
    pub tls: Option<TlsConfig>,
}

/// Inline PEM material validated once at [`PolicyClient::new`] so a
/// misconfiguration fails fast at startup.
#[derive(Clone, Debug)]
pub struct TlsConfig {
    /// PEM-encoded client cert chain, leaf first.
    pub client_cert: String,
    /// PEM-encoded private key (PKCS#8 or RSA) matching `client_cert`.
    pub client_key: String,
    /// PEM-encoded CA bundle the client trusts. System roots are not loaded.
    pub server_ca: String,
}

#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("invalid URL: {0}")]
    InvalidUrl(String),
    #[error("unsupported URL scheme `{0}` (expected `https` or `unix`)")]
    UnsupportedScheme(String),
    #[error("missing TLS config: https URLs require client_cert/client_key/server_ca")]
    MissingTls,
    #[error("unix URL missing socket path")]
    MissingSocketPath,
    #[error(transparent)]
    Transport(#[from] TransportError),
}

/// Call [`Self::session`] to get a per-transaction [`PolicySession`].
#[derive(Clone, Debug)]
pub struct PolicyClient {
    transport: Transport,
    request_timeout: Duration,
    protocol_version: String,
    expected_protocol_version: Option<String>,
    bypass_from: HashSet<Address>,
}

impl PolicyClient {
    pub fn new(config: Config) -> Result<Self, BuildError> {
        let parsed = url::Url::parse(&config.url)
            .map_err(|e| BuildError::InvalidUrl(e.to_string()))?;
        let transport_config = match parsed.scheme() {
            "https" => {
                let tls = config.tls.ok_or(BuildError::MissingTls)?;
                TransportConfig::Https { url: parsed, tls }
            }
            "unix" => {
                let socket_path = parsed.path();
                if socket_path.is_empty() {
                    return Err(BuildError::MissingSocketPath);
                }
                TransportConfig::Unix {
                    socket_path: std::path::PathBuf::from(socket_path),
                }
            }
            other => return Err(BuildError::UnsupportedScheme(other.to_string())),
        };
        let transport = Transport::from_config(transport_config)?;
        Ok(Self {
            transport,
            request_timeout: config.request_timeout,
            protocol_version: config.protocol_version,
            expected_protocol_version: config.expected_protocol_version,
            bypass_from: config.bypass_from,
        })
    }

    /// Creates a per-transaction [`PolicySession`] with its own trace slot
    /// and pending-sender state. Use a separate session for each concurrent
    /// RPC simulation so their `begin_tx` / `finish_tx` hooks don't trample
    /// each other's captured frames.
    pub fn session(&self, access_type: AccessType) -> PolicySession {
        PolicySession {
            client: self.clone(),
            slot: tracer::new_slot(),
            pending_tx_from: Arc::new(Mutex::new(None)),
            access_type,
        }
    }
}

/// Per-transaction state: trace slot, pending sender, and caller intent.
/// Implements [`TxValidator`] for both block-build and RPC simulation paths.
pub struct PolicySession {
    client: PolicyClient,
    slot: TraceSlot,
    pending_tx_from: Arc<Mutex<Option<Address>>>,
    access_type: AccessType,
}

impl PolicySession {
    /// Construct the [`Tracer`] paired with this session. The tracer writes
    /// captured frames into this session's slot; `finish_tx` reads them and
    /// POSTs `/judge`.
    pub fn paired_tracer(&self) -> Tracer {
        Tracer::new(self.slot.clone())
    }

    async fn admit(&self, ctx: &BeginTxContext<'_>) -> TxValidationResult {
        if self.client.bypass_from.contains(&ctx.from) {
            POLICY_CLIENT_METRICS.admit_bypassed.inc();
            return Ok(());
        }
        let request =
            AdmitRequest::from_context(ctx, &self.client.protocol_version, self.access_type);
        let started = Instant::now();
        let result = self.post_and_parse(Endpoint::Admit, &request).await;
        POLICY_CLIENT_METRICS.admit_latency.observe(started.elapsed());
        match result {
            Ok(true) => {
                POLICY_CLIENT_METRICS.admit_decisions[&Outcome::Allow].inc();
                Ok(())
            }
            Ok(false) => {
                POLICY_CLIENT_METRICS.admit_decisions[&Outcome::Deny].inc();
                Err(InvalidTransaction::FilteredByValidator)
            }
            Err(err) => {
                POLICY_CLIENT_METRICS.admit_errors[&classify_error(&err)].inc();
                Err(InvalidTransaction::FilteredByValidator)
            }
        }
    }

    async fn judge(&self, from: Option<Address>, frames: Vec<CapturedFrame>) -> TxValidationResult {
        if let Some(from) = from
            && self.client.bypass_from.contains(&from)
        {
            POLICY_CLIENT_METRICS.judge_bypassed.inc();
            return Ok(());
        }
        let request =
            JudgeRequest::new(&self.client.protocol_version, from, &frames, self.access_type);
        let started = Instant::now();
        let result = self.post_and_parse(Endpoint::Judge, &request).await;
        POLICY_CLIENT_METRICS.judge_latency.observe(started.elapsed());
        match result {
            Ok(true) => {
                POLICY_CLIENT_METRICS.judge_decisions[&Outcome::Allow].inc();
                Ok(())
            }
            Ok(false) => {
                POLICY_CLIENT_METRICS.judge_decisions[&Outcome::Deny].inc();
                Err(InvalidTransaction::FilteredByValidator)
            }
            Err(err) => {
                POLICY_CLIENT_METRICS.judge_errors[&classify_error(&err)].inc();
                Err(InvalidTransaction::FilteredByValidator)
            }
        }
    }

    async fn post_and_parse<R: Serialize>(
        &self,
        endpoint: Endpoint,
        request: &R,
    ) -> Result<bool, TransportError> {
        let body = serde_json::to_vec(request).expect("policy request serialization is infallible");
        let timeout = self.client.request_timeout;
        let response = match endpoint {
            Endpoint::Admit => {
                tokio::time::timeout(timeout, self.client.transport.post_admit(body)).await
            }
            Endpoint::Judge => {
                tokio::time::timeout(timeout, self.client.transport.post_judge(body)).await
            }
        };
        let raw = match response {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(err)) => {
                tracing::warn!(?err, ?endpoint, "policy request failed");
                return Err(err);
            }
            Err(_) => {
                tracing::warn!(?timeout, ?endpoint, "policy request timed out");
                return Err(TransportError::Timeout(timeout));
            }
        };
        let parsed: PolicyResponse = serde_json::from_slice(&raw).map_err(|err| {
            tracing::warn!(?err, ?endpoint, "policy response body malformed");
            TransportError::MalformedResponse
        })?;
        if let Some(expected) = &self.client.expected_protocol_version
            && parsed.protocol_version.as_deref() != Some(expected.as_str())
        {
            tracing::warn!(
                expected = %expected,
                got = ?parsed.protocol_version,
                ?endpoint,
                "policy response protocolVersion mismatch"
            );
            return Err(TransportError::ProtocolVersionMismatch);
        }
        if parsed.allow {
            Ok(true)
        } else {
            tracing::info!(
                rule_id = ?parsed.rule_id,
                reason = ?parsed.reason,
                ?endpoint,
                "policy denied"
            );
            Ok(false)
        }
    }
}

#[derive(Copy, Clone, Debug)]
enum Endpoint {
    Admit,
    Judge,
}

impl AnyTxValidator for PolicySession {
    fn as_evm(&mut self) -> Option<&mut impl TxValidator> {
        Some(self)
    }
}

impl TxValidator for PolicySession {
    fn begin_tx(&mut self, ctx: &BeginTxContext<'_>) -> TxValidationResult {
        // Stash `from` so `finish_tx` can apply the same `bypass_from`
        // short-circuit.
        *self
            .pending_tx_from
            .lock()
            .expect("pending_tx_from mutex poisoned") = Some(ctx.from);
        tokio::runtime::Handle::current().block_on(self.admit(ctx))
    }

    fn finish_tx(&mut self) -> TxValidationResult {
        let frames = self
            .slot
            .lock()
            .expect("policy tracer slot mutex poisoned")
            .take_frames();
        let from = self
            .pending_tx_from
            .lock()
            .expect("pending_tx_from mutex poisoned")
            .take();
        tokio::runtime::Handle::current().block_on(self.judge(from, frames))
    }
}

fn classify_error(err: &TransportError) -> ErrorReason {
    match err {
        TransportError::Timeout(_) => ErrorReason::Timeout,
        TransportError::TlsConfig(_) => ErrorReason::Connect,
        TransportError::NonSuccessStatus(_) => ErrorReason::Status,
        TransportError::Request(e) => {
            if e.is_connect() {
                ErrorReason::Connect
            } else {
                ErrorReason::Http
            }
        }
        TransportError::MalformedResponse => ErrorReason::MalformedResponse,
        TransportError::ProtocolVersionMismatch => ErrorReason::ProtocolVersionMismatch,
    }
}

#[cfg(test)]
mod tests;
