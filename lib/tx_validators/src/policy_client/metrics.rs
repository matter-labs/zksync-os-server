use std::time::Duration;
use vise::{Buckets, Counter, EncodeLabelValue, Histogram, LabeledFamily, Metrics, Unit};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EncodeLabelValue)]
#[metrics(label = "outcome", rename_all = "snake_case")]
pub enum AdmitOutcome {
    Allow,
    Deny,
}

/// Cheap reason breakdown for errors. The goal is operator-legible buckets,
/// not a perfect 1:1 with `TransportError` variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EncodeLabelValue)]
#[metrics(label = "reason", rename_all = "snake_case")]
pub enum AdmitErrorReason {
    Timeout,
    Connect,
    Http,
    Status,
    MalformedResponse,
    ProtocolVersionMismatch,
    NoRuntime,
}

#[derive(Debug, Metrics)]
#[metrics(prefix = "policy_client_admit")]
pub struct PolicyClientMetrics {
    /// Count of admit decisions, broken down by allow / deny.
    #[metrics(labels = ["outcome"])]
    pub decisions: LabeledFamily<AdmitOutcome, Counter>,

    /// Count of admit errors (treated as fail-closed by the client).
    #[metrics(labels = ["reason"])]
    pub errors: LabeledFamily<AdmitErrorReason, Counter>,

    /// Count of txs whose `from` hit the bypass allowlist — no admit call
    /// was made.
    pub bypassed: Counter,

    /// Latency of the admit round trip.
    /// Buckets span sub-ms to ~1s to cover both healthy localhost/UDS and
    /// worst-case TCP under load.
    #[metrics(unit = Unit::Seconds, buckets = Buckets::exponential(0.0001..=1.0, 2.0))]
    pub latency: Histogram<Duration>,
}

#[vise::register]
pub(crate) static POLICY_CLIENT_METRICS: vise::Global<PolicyClientMetrics> = vise::Global::new();
