//! `PolicyClient` — the server-side `TxValidator` that defers transaction
//! decisions to a Prividium policy service over HTTP.
//!
//! Two-stage surface:
//!   - pre-execution admit (`begin_tx`): serializes a `BeginTxContext`,
//!     POSTs it to `${POLICY_SERVICE_URL}/admit`, and maps allow → `Ok(())`
//!     / deny → `Err(FilteredByValidator)`.
//!   - post-execution judge (`finish_tx`): drains the per-frame trace
//!     captured by the paired [`Tracer`], POSTs it to
//!     `${POLICY_SERVICE_URL}/judge`, and applies the same allow/deny
//!     mapping.
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
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use alloy::primitives::{Address, U256};
use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use zksync_os_interface::error::InvalidTransaction;
use zksync_os_interface::tracing::{
    AnyTxValidator, BeginTxContext, TxValidationResult, TxValidator,
};

use self::metrics::{
    AdmitErrorReason, AdmitOutcome, JudgeErrorReason, JudgeOutcome, POLICY_CLIENT_METRICS,
};
pub use self::tracer::Tracer;
use self::tracer::{CapturedFrame, TraceSlot};
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
    /// Per-tx scratchpad shared with the paired [`Tracer`]. The tracer pushes
    /// frames during execution; `finish_tx` drains them. Mutex contention is
    /// structurally zero — only one block-build task ever writes to this slot,
    /// and RPC's async [`PolicyClient::admit`] path never touches it.
    trace_slot: TraceSlot,
    /// Recorded by `TxValidator::begin_tx` and consumed by `finish_tx` so the
    /// `bypass_from` short-circuit can apply to `/judge` with the same `from`
    /// the `/admit` call saw. Same single-task-only contract as `trace_slot`.
    pending_tx_from: Mutex<Option<Address>>,
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
                pending_tx_from: Mutex::new(None),
            }),
        })
    }

    /// Construct the [`Tracer`] paired with this client. Must be threaded
    /// through `run_block` alongside this client on the same task — they
    /// share a per-instance scratch slot that the tracer fills in during
    /// execution and `finish_tx` drains.
    pub fn paired_tracer(&self) -> Tracer {
        Tracer::new(self.inner.trace_slot.clone())
    }

    /// Consult the policy service. Used directly by RPC handlers; the sync
    /// [`TxValidator::begin_tx`] shim wraps this with `Handle::block_on`
    /// for the block-build path.
    pub async fn admit(&self, ctx: &BeginTxContext<'_>) -> TxValidationResult {
        if self.inner.bypass_from.contains(&ctx.from) {
            POLICY_CLIENT_METRICS.admit_bypassed.inc();
            return Ok(());
        }
        let request = AdmitRequest::from_context(ctx, &self.inner.protocol_version);
        let started = Instant::now();
        let outcome = self.admit_http(request).await;
        POLICY_CLIENT_METRICS
            .admit_latency
            .observe(started.elapsed());
        record_admit_outcome(&outcome);
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
                return Err(classify_admit_transport_error(&err));
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

    /// Post-execution judge call. Mirrors [`Self::admit`] in transport,
    /// timeout, auth, and protocol-version semantics; differs only in the
    /// request body (a flat list of captured execution frames) and the
    /// endpoint (`/judge`).
    async fn judge(&self, from: Option<Address>, frames: Vec<CapturedFrame>) -> TxValidationResult {
        if let Some(from) = from
            && self.inner.bypass_from.contains(&from)
        {
            POLICY_CLIENT_METRICS.judge_bypassed.inc();
            return Ok(());
        }
        let request = JudgeRequest::new(&self.inner.protocol_version, &frames);
        let started = Instant::now();
        let outcome = self.judge_http(&request).await;
        POLICY_CLIENT_METRICS
            .judge_latency
            .observe(started.elapsed());
        record_judge_outcome(&outcome);
        outcome.map_err(|_| InvalidTransaction::FilteredByValidator)
    }

    async fn judge_http(&self, request: &JudgeRequest<'_>) -> Result<(), JudgeOutcomeErr> {
        let body = serde_json::to_vec(request).map_err(|err| {
            tracing::error!(?err, "failed to serialize judge request — failing closed");
            JudgeOutcomeErr::MalformedResponse
        })?;
        let transport = self.inner.transport.clone();
        let timeout = self.inner.request_timeout;
        let response = tokio::time::timeout(timeout, transport.post_judge(body)).await;
        let raw = match response {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(err)) => {
                tracing::warn!(?err, "judge request failed — failing closed");
                return Err(classify_judge_transport_error(&err));
            }
            Err(_) => {
                tracing::warn!(?timeout, "judge request timed out — failing closed");
                return Err(JudgeOutcomeErr::Timeout);
            }
        };
        let parsed: JudgeResponse = serde_json::from_slice(&raw).map_err(|err| {
            tracing::warn!(?err, "judge response body malformed — failing closed");
            JudgeOutcomeErr::MalformedResponse
        })?;
        if let Some(floor) = &self.inner.min_protocol_version
            && parsed.protocol_version.as_deref() != Some(floor.as_str())
        {
            tracing::warn!(
                expected = %floor,
                got = ?parsed.protocol_version,
                "judge response protocolVersion mismatch — failing closed"
            );
            return Err(JudgeOutcomeErr::ProtocolVersionMismatch);
        }
        if parsed.allow {
            Ok(())
        } else {
            tracing::info!(
                rule_id = ?parsed.rule_id,
                reason = ?parsed.reason,
                "judge denied by policy service"
            );
            Err(JudgeOutcomeErr::Denied)
        }
    }
}

fn record_admit_outcome(outcome: &Result<(), AdmitOutcomeErr>) {
    match outcome {
        Ok(()) => {
            POLICY_CLIENT_METRICS.admit_decisions[&AdmitOutcome::Allow].inc();
        }
        Err(AdmitOutcomeErr::Denied) => {
            POLICY_CLIENT_METRICS.admit_decisions[&AdmitOutcome::Deny].inc();
        }
        Err(reason) => {
            POLICY_CLIENT_METRICS.admit_errors[&reason.error_label()].inc();
        }
    }
}

fn record_judge_outcome(outcome: &Result<(), JudgeOutcomeErr>) {
    match outcome {
        Ok(()) => {
            POLICY_CLIENT_METRICS.judge_decisions[&JudgeOutcome::Allow].inc();
        }
        Err(JudgeOutcomeErr::Denied) => {
            POLICY_CLIENT_METRICS.judge_decisions[&JudgeOutcome::Deny].inc();
        }
        Err(reason) => {
            POLICY_CLIENT_METRICS.judge_errors[&reason.error_label()].inc();
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
                POLICY_CLIENT_METRICS.admit_errors[&AdmitErrorReason::NoRuntime].inc();
                return Err(InvalidTransaction::FilteredByValidator);
            }
        };
        // Stash `from` so `finish_tx` can apply the same `bypass_from`
        // short-circuit without re-deriving it from the captured trace.
        *self
            .inner
            .pending_tx_from
            .lock()
            .expect("pending_tx_from mutex poisoned") = Some(ctx.from);
        handle.block_on(self.admit(ctx))
    }

    fn finish_tx(&mut self) -> TxValidationResult {
        let frames = self
            .inner
            .trace_slot
            .lock()
            .expect("policy tracer slot mutex poisoned")
            .take_frames();
        let from = self
            .inner
            .pending_tx_from
            .lock()
            .expect("pending_tx_from mutex poisoned")
            .take();

        let handle = match tokio::runtime::Handle::try_current() {
            Ok(handle) => handle,
            Err(_) => {
                tracing::error!("PolicyClient invoked outside a tokio runtime — failing closed");
                POLICY_CLIENT_METRICS.judge_errors[&JudgeErrorReason::NoRuntime].inc();
                return Err(InvalidTransaction::FilteredByValidator);
            }
        };
        handle.block_on(self.judge(from, frames))
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

/// JSON body POSTed to `/judge`. Mirrors `AdmitRequest`'s wire shape but
/// carries the per-frame execution trace instead of the static `BeginTxContext`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct JudgeRequest<'a> {
    pub protocol_version: &'a str,
    pub trace: JudgeTrace<'a>,
}

#[derive(Debug, Serialize)]
pub(crate) struct JudgeTrace<'a> {
    pub frames: Vec<JudgeFrame<'a>>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct JudgeFrame<'a> {
    pub caller: Address,
    pub callee: Address,
    pub value: U256,
    #[serde(with = "alloy::hex")]
    pub calldata: &'a [u8],
    pub deploys: &'a [Address],
}

impl<'a> JudgeRequest<'a> {
    fn new(protocol_version: &'a str, frames: &'a [CapturedFrame]) -> Self {
        Self {
            protocol_version,
            trace: JudgeTrace {
                frames: frames
                    .iter()
                    .map(|f| JudgeFrame {
                        caller: f.caller,
                        callee: f.callee,
                        value: f.value,
                        calldata: &f.calldata,
                        deploys: &f.deploys,
                    })
                    .collect(),
            },
        }
    }
}

/// Identical to `AdmitResponse` today — kept distinct so the two endpoints
/// can diverge without rippling through the admit code path.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct JudgeResponse {
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

/// Mirror of [`AdmitOutcomeErr`] for the judge path. Kept separate so labels
/// land on the right metric family without the call sites needing to
/// disambiguate.
#[derive(Debug)]
enum JudgeOutcomeErr {
    Denied,
    Timeout,
    Connect,
    Http,
    Status,
    MalformedResponse,
    ProtocolVersionMismatch,
}

impl JudgeOutcomeErr {
    fn error_label(&self) -> JudgeErrorReason {
        match self {
            Self::Denied => unreachable!("denied is counted as a decision, not an error"),
            Self::Timeout => JudgeErrorReason::Timeout,
            Self::Connect => JudgeErrorReason::Connect,
            Self::Http => JudgeErrorReason::Http,
            Self::Status => JudgeErrorReason::Status,
            Self::MalformedResponse => JudgeErrorReason::MalformedResponse,
            Self::ProtocolVersionMismatch => JudgeErrorReason::ProtocolVersionMismatch,
        }
    }
}

fn classify_admit_transport_error(err: &TransportError) -> AdmitOutcomeErr {
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

fn classify_judge_transport_error(err: &TransportError) -> JudgeOutcomeErr {
    match err {
        TransportError::Timeout(_) => JudgeOutcomeErr::Timeout,
        TransportError::Connect(_) => JudgeOutcomeErr::Connect,
        TransportError::NonSuccessStatus(_) => JudgeOutcomeErr::Status,
        TransportError::Hyper(_)
        | TransportError::HttpClient(_)
        | TransportError::BuildRequest(_) => JudgeOutcomeErr::Http,
        TransportError::InvalidUrl(_) | TransportError::UnsupportedScheme(_) => {
            JudgeOutcomeErr::Connect
        }
    }
}

#[cfg(test)]
mod tests;
