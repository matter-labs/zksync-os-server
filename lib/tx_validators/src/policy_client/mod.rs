//! `TxValidator` that delegates each transaction decision to an external
//! HTTP policy service. Any transport, protocol-version, or response
//! parsing error is fail-closed.

mod metrics;
mod tracer;
mod transport;
mod wire;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use alloy::primitives::Address;
use serde::Serialize;
use zksync_os_interface::error::InvalidTransaction;
use zksync_os_interface::tracing::{
    AnyTxValidator, BeginTxContext, TxValidationResult, TxValidator,
};

use self::metrics::{
    AdmitErrorReason, AdmitOutcome, JudgeErrorReason, JudgeOutcome, POLICY_CLIENT_METRICS,
};
use self::tracer::TraceSlot;
pub use self::tracer::{CallKind, CapturedFrame, Tracer};
use self::transport::{Transport, TransportError};
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
    /// mTLS material. Required for `https://`; must be `None` for `unix:///`.
    pub tls: Option<TlsConfig>,
    /// Caller intent reported through the `TxValidator` trait. Block-build
    /// uses `Write`; RPC paths fork with the right intent per call.
    pub access_type: AccessType,
}

/// PEM paths read once at [`PolicyClient::new`] so a misconfiguration
/// fails fast at startup.
#[derive(Clone, Debug)]
pub struct TlsConfig {
    /// PEM-encoded client cert chain, leaf first.
    pub client_cert: PathBuf,
    /// PEM-encoded private key (PKCS#8 or RSA) matching `client_cert`.
    pub client_key: PathBuf,
    /// PEM-encoded CA bundle the client trusts. System roots are not loaded.
    pub server_ca: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error(transparent)]
    Transport(#[from] TransportError),
}

/// `Clone` shares the slot, `pending_tx_from`, and `access_type` (used by
/// block-build, where the same client is reused across txs). [`Self::fork`]
/// returns a sibling sharing the transport pool but with fresh per-tx
/// state and a chosen access type, used by RPC so concurrent simulations
/// don't trample each other's captured frames.
// `Clone` shares the per-tx slot — use [`Self::fork`] for concurrent
// simulations that must not see each other's frames.
#[derive(Clone)]
pub struct PolicyClient {
    shared: Arc<Shared>,
    slot: TraceSlot,
    pending_tx_from: Arc<Mutex<Option<Address>>>,
    access_type: AccessType,
}

impl std::fmt::Debug for PolicyClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PolicyClient")
            .field("request_timeout", &self.shared.request_timeout)
            .field("protocol_version", &self.shared.protocol_version)
            .field(
                "expected_protocol_version",
                &self.shared.expected_protocol_version,
            )
            .finish_non_exhaustive()
    }
}

struct Shared {
    transport: Transport,
    request_timeout: Duration,
    protocol_version: String,
    expected_protocol_version: Option<String>,
    bypass_from: HashSet<Address>,
}

impl PolicyClient {
    pub fn new(config: Config) -> Result<Self, BuildError> {
        let transport = Transport::from_url(&config.url, config.tls.as_ref())?;
        Ok(Self {
            shared: Arc::new(Shared {
                transport,
                request_timeout: config.request_timeout,
                protocol_version: config.protocol_version,
                expected_protocol_version: config.expected_protocol_version,
                bypass_from: config.bypass_from,
            }),
            slot: tracer::new_slot(),
            pending_tx_from: Arc::new(Mutex::new(None)),
            access_type: config.access_type,
        })
    }

    /// Sibling client that shares the same transport / config but owns a
    /// fresh per-tx scratch slot and `pending_tx_from`, with the supplied
    /// access type. Use this for each concurrent RPC simulation so the
    /// validator's `begin_tx` / `finish_tx` hooks fired by `simulate_tx`
    /// see only this call's frames and ship the right intent.
    pub fn fork(&self, access_type: AccessType) -> Self {
        Self {
            shared: Arc::clone(&self.shared),
            slot: tracer::new_slot(),
            pending_tx_from: Arc::new(Mutex::new(None)),
            access_type,
        }
    }

    /// Construct the [`Tracer`] paired with this client. The tracer writes
    /// captured frames into this client's slot; `validator.finish_tx`
    /// (fired by the bootloader after EVM execution) reads them and POSTs
    /// `/judge`.
    pub fn paired_tracer(&self) -> Tracer {
        Tracer::new(self.slot.clone())
    }

    /// Pre-execution call. Async surface used by RPC handlers; block-build
    /// reaches it through the sync [`TxValidator::begin_tx`] bridge.
    pub async fn admit(
        &self,
        ctx: &BeginTxContext<'_>,
        access_type: AccessType,
    ) -> TxValidationResult {
        if self.shared.bypass_from.contains(&ctx.from) {
            POLICY_CLIENT_METRICS.admit_bypassed.inc();
            return Ok(());
        }
        let request = AdmitRequest::from_context(ctx, &self.shared.protocol_version, access_type);
        let started = Instant::now();
        let outcome = self.post_and_parse(Endpoint::Admit, &request).await;
        POLICY_CLIENT_METRICS
            .admit_latency
            .observe(started.elapsed());
        record_admit_outcome(&outcome);
        outcome.map_err(|_| InvalidTransaction::FilteredByValidator)
    }

    /// Post-execution call. Same surface as [`Self::admit`], but ships the
    /// captured execution trace.
    pub async fn judge(
        &self,
        from: Option<Address>,
        frames: Vec<CapturedFrame>,
        access_type: AccessType,
    ) -> TxValidationResult {
        if let Some(from) = from
            && self.shared.bypass_from.contains(&from)
        {
            POLICY_CLIENT_METRICS.judge_bypassed.inc();
            return Ok(());
        }
        let request = JudgeRequest::new(&self.shared.protocol_version, from, &frames, access_type);
        let started = Instant::now();
        let outcome = self.post_and_parse(Endpoint::Judge, &request).await;
        POLICY_CLIENT_METRICS
            .judge_latency
            .observe(started.elapsed());
        record_judge_outcome(&outcome);
        outcome.map_err(|_| InvalidTransaction::FilteredByValidator)
    }

    async fn post_and_parse<R: Serialize>(
        &self,
        endpoint: Endpoint,
        request: &R,
    ) -> Result<(), OutcomeErr> {
        let body = serde_json::to_vec(request).map_err(|err| {
            tracing::error!(?err, ?endpoint, "failed to serialize policy request");
            OutcomeErr::MalformedResponse
        })?;
        let timeout = self.shared.request_timeout;
        let response = match endpoint {
            Endpoint::Admit => {
                tokio::time::timeout(timeout, self.shared.transport.post_admit(body)).await
            }
            Endpoint::Judge => {
                tokio::time::timeout(timeout, self.shared.transport.post_judge(body)).await
            }
        };
        let raw = match response {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(err)) => {
                tracing::warn!(?err, ?endpoint, "policy request failed");
                return Err(classify_transport_error(&err));
            }
            Err(_) => {
                tracing::warn!(?timeout, ?endpoint, "policy request timed out");
                return Err(OutcomeErr::Timeout);
            }
        };
        let parsed: PolicyResponse = serde_json::from_slice(&raw).map_err(|err| {
            tracing::warn!(?err, ?endpoint, "policy response body malformed");
            OutcomeErr::MalformedResponse
        })?;
        if let Some(expected) = &self.shared.expected_protocol_version
            && parsed.protocol_version.as_deref() != Some(expected.as_str())
        {
            tracing::warn!(
                expected = %expected,
                got = ?parsed.protocol_version,
                ?endpoint,
                "policy response protocolVersion mismatch"
            );
            return Err(OutcomeErr::ProtocolVersionMismatch);
        }
        if parsed.allow {
            Ok(())
        } else {
            tracing::info!(
                rule_id = ?parsed.rule_id,
                reason = ?parsed.reason,
                ?endpoint,
                "policy denied"
            );
            Err(OutcomeErr::Denied)
        }
    }
}

#[derive(Copy, Clone, Debug)]
enum Endpoint {
    Admit,
    Judge,
}

fn record_admit_outcome(outcome: &Result<(), OutcomeErr>) {
    match outcome {
        Ok(()) => {
            POLICY_CLIENT_METRICS.admit_decisions[&AdmitOutcome::Allow].inc();
        }
        Err(OutcomeErr::Denied) => {
            POLICY_CLIENT_METRICS.admit_decisions[&AdmitOutcome::Deny].inc();
        }
        Err(reason) => {
            if let Some(label) = reason.admit_label() {
                POLICY_CLIENT_METRICS.admit_errors[&label].inc();
            }
        }
    }
}

fn record_judge_outcome(outcome: &Result<(), OutcomeErr>) {
    match outcome {
        Ok(()) => {
            POLICY_CLIENT_METRICS.judge_decisions[&JudgeOutcome::Allow].inc();
        }
        Err(OutcomeErr::Denied) => {
            POLICY_CLIENT_METRICS.judge_decisions[&JudgeOutcome::Deny].inc();
        }
        Err(reason) => {
            if let Some(label) = reason.judge_label() {
                POLICY_CLIENT_METRICS.judge_errors[&label].inc();
            }
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
        let handle = match tokio::runtime::Handle::try_current() {
            Ok(handle) => handle,
            Err(_) => {
                tracing::error!("PolicyClient called outside a tokio runtime");
                POLICY_CLIENT_METRICS.admit_errors[&AdmitErrorReason::NoRuntime].inc();
                return Err(InvalidTransaction::FilteredByValidator);
            }
        };
        // Stash `from` so `finish_tx` can apply the same `bypass_from`
        // short-circuit.
        *self
            .pending_tx_from
            .lock()
            .expect("pending_tx_from mutex poisoned") = Some(ctx.from);
        handle.block_on(self.admit(ctx, self.access_type))
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

        let handle = match tokio::runtime::Handle::try_current() {
            Ok(handle) => handle,
            Err(_) => {
                tracing::error!("PolicyClient called outside a tokio runtime");
                POLICY_CLIENT_METRICS.judge_errors[&JudgeErrorReason::NoRuntime].inc();
                return Err(InvalidTransaction::FilteredByValidator);
            }
        };
        handle.block_on(self.judge(from, frames, self.access_type))
    }
}

/// Failure modes from a single HTTP call. `Denied` is counted as a
/// decision; the rest land in the per-endpoint error metric.
#[derive(Debug)]
enum OutcomeErr {
    Denied,
    Timeout,
    Connect,
    Http,
    Status,
    MalformedResponse,
    ProtocolVersionMismatch,
}

impl OutcomeErr {
    fn admit_label(&self) -> Option<AdmitErrorReason> {
        Some(match self {
            Self::Denied => return None,
            Self::Timeout => AdmitErrorReason::Timeout,
            Self::Connect => AdmitErrorReason::Connect,
            Self::Http => AdmitErrorReason::Http,
            Self::Status => AdmitErrorReason::Status,
            Self::MalformedResponse => AdmitErrorReason::MalformedResponse,
            Self::ProtocolVersionMismatch => AdmitErrorReason::ProtocolVersionMismatch,
        })
    }

    fn judge_label(&self) -> Option<JudgeErrorReason> {
        Some(match self {
            Self::Denied => return None,
            Self::Timeout => JudgeErrorReason::Timeout,
            Self::Connect => JudgeErrorReason::Connect,
            Self::Http => JudgeErrorReason::Http,
            Self::Status => JudgeErrorReason::Status,
            Self::MalformedResponse => JudgeErrorReason::MalformedResponse,
            Self::ProtocolVersionMismatch => JudgeErrorReason::ProtocolVersionMismatch,
        })
    }
}

fn classify_transport_error(err: &TransportError) -> OutcomeErr {
    match err {
        TransportError::Timeout(_) => OutcomeErr::Timeout,
        TransportError::Connect(_) => OutcomeErr::Connect,
        TransportError::NonSuccessStatus(_) => OutcomeErr::Status,
        TransportError::Hyper(_)
        | TransportError::HttpClient(_)
        | TransportError::BuildRequest(_) => OutcomeErr::Http,
        // URL/TLS errors are construction-time and shouldn't reach here.
        // Fold into `Connect` to stay fail-closed if config changed under us.
        TransportError::InvalidUrl(_)
        | TransportError::UnsupportedScheme(_)
        | TransportError::MissingTls
        | TransportError::UnexpectedTls
        | TransportError::TlsRead { .. }
        | TransportError::TlsParse { .. }
        | TransportError::TlsConfig(_) => OutcomeErr::Connect,
    }
}

#[cfg(test)]
mod tests;
