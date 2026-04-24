//! `PolicyClient` — the server-side `TxValidator` that defers transaction
//! decisions to a Prividium policy service over HTTP.
//!
//! The intended surface is two-stage:
//!   - pre-execution admit (`begin_tx`) — currently implemented: serializes
//!     a `BeginTxContext`, POSTs it to `${POLICY_SERVICE_URL}/admit`, and
//!     maps allow → `Ok(())` / deny → `Err(FilteredByValidator)`.
//!   - post-execution judge (`finish_tx`) — stubbed `Ok(())` for now. The
//!     paired [`Tracer`] already captures per-frame data into a shared slot
//!     so a follow-up commit can drain it into a `/judge` call with no
//!     further plumbing changes.
//!
//! Any error on a policy call (timeout, connection failure, non-2xx,
//! malformed body, protocol-version mismatch) is fail-closed.
//!
//! HTTP-over-TCP and HTTP-over-UDS share the `PolicyClient` surface and
//! differ only in the URL scheme (`http://` vs `unix:///`).
//!
//! TODO: reconcile request/response shapes with the Prividium OpenAPI spec
//! once it's available — the types below are a first pass.

mod metrics;
mod tracer;
mod transport;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use alloy::primitives::{Address, U256};
use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use zksync_os_interface::error::InvalidTransaction;
use zksync_os_interface::tracing::{
    AnyTxValidator, BeginTxContext, TxValidationResult, TxValidator,
};

use self::metrics::{AdmitErrorReason, AdmitOutcome, POLICY_CLIENT_METRICS};
pub use self::tracer::Tracer;
use self::tracer::TraceSlot;
use self::transport::{Transport, TransportError};

/// Plain config struct — mirrors the fields `node/bin` reads out of env vars
/// / config files and passes down to the sequencer.
#[derive(Clone, Debug)]
pub struct Config {
    /// `http://host:port` or `unix:///path/to.sock`.
    pub url: String,
    /// Per-request timeout for any call to the policy service.
    pub request_timeout: Duration,
    /// Bearer token sent in the `Authorization` header. `None` means no auth.
    pub auth_token: Option<SecretString>,
    /// Protocol version this server advertises on every policy request.
    pub protocol_version: String,
    /// If set, any policy response whose `protocolVersion` is less than this
    /// is rejected (fail-closed). Independent from `protocol_version`, which
    /// is what we *send*; this is the floor we *accept*.
    pub min_protocol_version: Option<String>,
    /// Source addresses whose txs skip the policy service entirely — no
    /// admit or judge call is made. Intended for protocol-internal senders
    /// (bootloader, force-deployer, ...) whose txs the chain cannot let the
    /// service refuse without bricking startup.
    pub bypass_from: HashSet<Address>,
}

#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error(transparent)]
    Transport(#[from] TransportError),
}

/// Clone-cheap handle to the underlying HTTP client and config.
#[derive(Clone)]
pub struct PolicyClient {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for PolicyClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PolicyClient")
            .field("request_timeout", &self.inner.request_timeout)
            .field("protocol_version", &self.inner.protocol_version)
            .field("min_protocol_version", &self.inner.min_protocol_version)
            .finish_non_exhaustive()
    }
}

struct Inner {
    transport: Transport,
    request_timeout: Duration,
    protocol_version: String,
    min_protocol_version: Option<String>,
    bypass_from: HashSet<Address>,
    /// Per-tx scratchpad shared with the paired [`Tracer`]. The tracer
    /// appends frames during execution; the post-execution judge call will
    /// drain them in a follow-up commit. Mutex contention is structurally
    /// zero — only one block-build task ever writes to this slot, and
    /// RPC's async [`PolicyClient::admit`] path never touches it.
    trace_slot: TraceSlot,
}

impl PolicyClient {
    pub fn new(config: Config) -> Result<Self, BuildError> {
        let transport = Transport::from_url(&config.url, config.auth_token.clone())?;
        Ok(Self {
            inner: Arc::new(Inner {
                transport,
                request_timeout: config.request_timeout,
                protocol_version: config.protocol_version,
                min_protocol_version: config.min_protocol_version,
                bypass_from: config.bypass_from,
                trace_slot: tracer::new_slot(),
            }),
        })
    }

    /// Construct the [`Tracer`] paired with this client. Must be threaded
    /// through `run_block` alongside this client on the same task — they
    /// share a per-instance scratch slot that the tracer fills in during
    /// execution.
    pub fn paired_tracer(&self) -> Tracer {
        Tracer::new(self.inner.trace_slot.clone())
    }

    /// Consult the policy service. Used directly by RPC handlers; the sync
    /// [`TxValidator::begin_tx`] shim wraps this with `Handle::block_on`
    /// for the block-build path.
    pub async fn admit(&self, ctx: &BeginTxContext<'_>) -> TxValidationResult {
        if self.inner.bypass_from.contains(&ctx.from) {
            POLICY_CLIENT_METRICS.bypassed.inc();
            return Ok(());
        }
        let request = AdmitRequest::from_context(ctx, &self.inner.protocol_version);
        let started = Instant::now();
        let outcome = self.admit_http(request).await;
        POLICY_CLIENT_METRICS.latency.observe(started.elapsed());
        record_outcome(&outcome);
        outcome.map_err(|_| InvalidTransaction::FilteredByValidator)
    }

    async fn admit_http(&self, request: AdmitRequest<'_>) -> Result<(), AdmitOutcomeErr> {
        let body = serde_json::to_vec(&request).map_err(|err| {
            tracing::error!(?err, "failed to serialize admit request — failing closed");
            AdmitOutcomeErr::MalformedResponse
        })?;
        let transport = self.inner.transport.clone();
        let timeout = self.inner.request_timeout;
        let response = tokio::time::timeout(timeout, transport.post_admit(body)).await;
        let raw = match response {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(err)) => {
                tracing::warn!(?err, "admit request failed — failing closed");
                return Err(classify_transport_error(&err));
            }
            Err(_) => {
                tracing::warn!(?timeout, "admit request timed out — failing closed");
                return Err(AdmitOutcomeErr::Timeout);
            }
        };
        let parsed: AdmitResponse = serde_json::from_slice(&raw).map_err(|err| {
            tracing::warn!(?err, "admit response body malformed — failing closed");
            AdmitOutcomeErr::MalformedResponse
        })?;
        if let Some(floor) = &self.inner.min_protocol_version
            && parsed.protocol_version.as_deref() != Some(floor.as_str())
        {
            // Exact-match for now — until the `protocolVersion` encoding
            // (semver vs monotone int) is settled, stricter is safer.
            tracing::warn!(
                expected = %floor,
                got = ?parsed.protocol_version,
                "admit response protocolVersion mismatch — failing closed"
            );
            return Err(AdmitOutcomeErr::ProtocolVersionMismatch);
        }
        if parsed.allow {
            Ok(())
        } else {
            tracing::info!(
                rule_id = ?parsed.rule_id,
                reason = ?parsed.reason,
                "admit denied by policy service"
            );
            Err(AdmitOutcomeErr::Denied)
        }
    }
}

fn record_outcome(outcome: &Result<(), AdmitOutcomeErr>) {
    match outcome {
        Ok(()) => {
            POLICY_CLIENT_METRICS.decisions[&AdmitOutcome::Allow].inc();
        }
        Err(AdmitOutcomeErr::Denied) => {
            POLICY_CLIENT_METRICS.decisions[&AdmitOutcome::Deny].inc();
        }
        Err(reason) => {
            POLICY_CLIENT_METRICS.errors[&reason.error_label()].inc();
        }
    }
}

impl AnyTxValidator for PolicyClient {
    fn as_evm(&mut self) -> Option<&mut impl TxValidator> {
        Some(self)
    }
}

impl TxValidator for PolicyClient {
    fn begin_tx(&mut self, ctx: &BeginTxContext<'_>) -> TxValidationResult {
        // Block-build drives `PolicyClient` from `spawn_blocking`, so
        // `Handle::block_on` is the correct bridge into the async admit
        // path. Outside a tokio context there's nothing to block on —
        // fail closed and record it so the deployment can spot it.
        let handle = match tokio::runtime::Handle::try_current() {
            Ok(handle) => handle,
            Err(_) => {
                tracing::error!("PolicyClient invoked outside a tokio runtime — failing closed");
                POLICY_CLIENT_METRICS.errors[&AdmitErrorReason::NoRuntime].inc();
                return Err(InvalidTransaction::FilteredByValidator);
            }
        };
        handle.block_on(self.admit(ctx))
    }

    fn finish_tx(&mut self) -> TxValidationResult {
        // TODO: wire the execution-result judge, and apply the same
        // `bypass_from` check here (requires stashing the begin_tx context).
        Ok(())
    }
}

/// JSON body POSTed to `/admit`. Field names match what the TS policy service
/// will expose.
/// TODO: confirm against the Prividium OpenAPI spec once it's available.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AdmitRequest<'a> {
    pub protocol_version: &'a str,
    pub from: Address,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<Address>,
    pub value: U256,
    #[serde(with = "alloy::hex")]
    pub calldata: &'a [u8],
    pub gas_limit: u64,
}

impl<'a> AdmitRequest<'a> {
    fn from_context(ctx: &'a BeginTxContext<'a>, protocol_version: &'a str) -> Self {
        Self {
            protocol_version,
            from: ctx.from,
            to: ctx.to,
            value: ctx.value,
            calldata: ctx.calldata,
            gas_limit: ctx.gas_limit,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AdmitResponse {
    pub allow: bool,
    #[serde(default)]
    pub rule_id: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub protocol_version: Option<String>,
}

/// Internal error carrier so `admit` can keep one code path for metrics +
/// logging without leaking details to the caller. `NoRuntime` is recorded
/// separately from `begin_tx`; everything here is an HTTP-path outcome.
#[derive(Debug)]
enum AdmitOutcomeErr {
    Denied,
    Timeout,
    Connect,
    Http,
    Status,
    MalformedResponse,
    ProtocolVersionMismatch,
}

impl AdmitOutcomeErr {
    fn error_label(&self) -> AdmitErrorReason {
        match self {
            Self::Denied => unreachable!("denied is counted as a decision, not an error"),
            Self::Timeout => AdmitErrorReason::Timeout,
            Self::Connect => AdmitErrorReason::Connect,
            Self::Http => AdmitErrorReason::Http,
            Self::Status => AdmitErrorReason::Status,
            Self::MalformedResponse => AdmitErrorReason::MalformedResponse,
            Self::ProtocolVersionMismatch => AdmitErrorReason::ProtocolVersionMismatch,
        }
    }
}

fn classify_transport_error(err: &TransportError) -> AdmitOutcomeErr {
    match err {
        TransportError::Timeout(_) => AdmitOutcomeErr::Timeout,
        TransportError::Connect(_) => AdmitOutcomeErr::Connect,
        TransportError::NonSuccessStatus(_) => AdmitOutcomeErr::Status,
        TransportError::Hyper(_)
        | TransportError::HttpClient(_)
        | TransportError::BuildRequest(_) => AdmitOutcomeErr::Http,
        TransportError::InvalidUrl(_) | TransportError::UnsupportedScheme(_) => {
            // URL problems surface at construction time; if we reach here the
            // config changed under us, which is still a fail-closed scenario.
            AdmitOutcomeErr::Connect
        }
    }
}

#[cfg(test)]
mod tests;
