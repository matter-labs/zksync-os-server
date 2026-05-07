//! `TxValidator` that delegates each transaction decision to an external
//! HTTP policy service. Any transport, protocol-version, or response
//! parsing error is fail-closed.

mod metrics;
mod tracer;
mod transport;
mod wire;
#[cfg(test)]
mod tests;

use std::collections::HashSet;
use std::sync::Arc;
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
    /// `http://host:port` or `unix:///path/to.sock`.
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
    /// Bearer token sent as `Authorization: Bearer <token>` on every request.
    /// Set for TCP transports; leave `None` when using `unix://` on a
    /// network-isolated host where the socket path is the access control.
    pub auth_token: Option<String>,
}


#[derive(Debug)]
struct PolicyClientInner {
    transport: Transport,
    request_timeout: Duration,
    protocol_version: String,
    expected_protocol_version: Option<String>,
    bypass_from: HashSet<Address>,
}

/// Call [`Self::session`] to get a per-transaction [`PolicySession`].
#[derive(Clone, Debug)]
pub struct PolicyClient(Arc<PolicyClientInner>);

impl PolicyClient {
    pub fn new(config: Config) -> anyhow::Result<Self> {
        let parsed = url::Url::parse(&config.url)
            .map_err(|e| anyhow::anyhow!("invalid policy service URL: {e}"))?;
        let transport_config = match parsed.scheme() {
            "http" => TransportConfig::Http {
                url: parsed,
                auth_token: config.auth_token.clone(),
            },
            "unix" => TransportConfig::Unix {
                socket_path: std::path::PathBuf::from(parsed.path()),
                auth_token: config.auth_token.clone(),
            },
            other => anyhow::bail!(
                "unsupported URL scheme `{other}` (expected `http` or `unix`)"
            ),
        };
        let transport = Transport::from_config(transport_config)
            .map_err(|e| anyhow::anyhow!("failed to build transport: {e}"))?;
        Ok(Self(Arc::new(PolicyClientInner {
            transport,
            request_timeout: config.request_timeout,
            protocol_version: config.protocol_version,
            expected_protocol_version: config.expected_protocol_version,
            bypass_from: config.bypass_from,
        })))
    }

    /// Creates a per-transaction [`PolicySession`] with its own trace slot
    /// and pending-sender state. Use a separate session for each concurrent
    /// RPC simulation so their `begin_tx` / `finish_tx` hooks don't trample
    /// each other's captured frames.
    pub fn session(&self, access_type: AccessType) -> PolicySession {
        PolicySession {
            client: Arc::clone(&self.0),
            slot: tracer::new_slot(),
            pending_tx_from: None,
            access_type,
        }
    }
}

/// Per-transaction state: trace slot, pending sender, and caller intent.
/// Implements [`TxValidator`] for both block-build and RPC simulation paths.
pub struct PolicySession {
    client: Arc<PolicyClientInner>,
    slot: TraceSlot,
    pending_tx_from: Option<Address>,
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

    async fn judge(&self, from: Option<Address>, root: Option<CapturedFrame>) -> TxValidationResult {
        if let Some(from) = from
            && self.client.bypass_from.contains(&from)
        {
            POLICY_CLIENT_METRICS.judge_bypassed.inc();
            return Ok(());
        }
        let request =
            JudgeRequest::new(&self.client.protocol_version, from, root.as_ref(), self.access_type);
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
        self.pending_tx_from = Some(ctx.from);
        tokio::runtime::Handle::current().block_on(self.admit(ctx))
    }

    fn finish_tx(&mut self) -> TxValidationResult {
        let root = self
            .slot
            .lock()
            .expect("policy tracer slot mutex poisoned")
            .take_root();
        let from = self.pending_tx_from.take();
        tokio::runtime::Handle::current().block_on(self.judge(from, root))
    }
}

fn classify_error(err: &TransportError) -> ErrorReason {
    match err {
        TransportError::Timeout(_) => ErrorReason::Timeout,
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
