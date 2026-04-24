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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EncodeLabelValue)]
#[metrics(label = "outcome", rename_all = "snake_case")]
pub enum JudgeOutcome {
    Allow,
    Deny,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EncodeLabelValue)]
#[metrics(label = "reason", rename_all = "snake_case")]
pub enum JudgeErrorReason {
    Timeout,
    Connect,
    Http,
    Status,
    MalformedResponse,
    ProtocolVersionMismatch,
    NoRuntime,
}

#[derive(Debug, Metrics)]
#[metrics(prefix = "policy_client")]
pub struct PolicyClientMetrics {
    /// Count of admit decisions, broken down by allow / deny.
    #[metrics(labels = ["outcome"])]
    pub admit_decisions: LabeledFamily<AdmitOutcome, Counter>,

    /// Count of admit errors (treated as fail-closed by the client).
    #[metrics(labels = ["reason"])]
    pub admit_errors: LabeledFamily<AdmitErrorReason, Counter>,

    /// Count of admit calls bypassed via the `bypass_from` allowlist.
    pub admit_bypassed: Counter,

    /// Latency of the admit round trip.
    /// Buckets span sub-ms to ~1s to cover both healthy localhost/UDS and
    /// worst-case TCP under load.
    #[metrics(unit = Unit::Seconds, buckets = Buckets::exponential(0.0001..=1.0, 2.0))]
    pub admit_latency: Histogram<Duration>,

    /// Count of judge decisions, broken down by allow / deny.
    #[metrics(labels = ["outcome"])]
    pub judge_decisions: LabeledFamily<JudgeOutcome, Counter>,

    /// Count of judge errors (treated as fail-closed by the client).
    #[metrics(labels = ["reason"])]
    pub judge_errors: LabeledFamily<JudgeErrorReason, Counter>,

    /// Count of judge calls bypassed via the `bypass_from` allowlist.
    pub judge_bypassed: Counter,

    /// Latency of the judge round trip. Same bucketing rationale as
    /// `admit_latency` — judge runs in the same hot path during block build.
    #[metrics(unit = Unit::Seconds, buckets = Buckets::exponential(0.0001..=1.0, 2.0))]
    pub judge_latency: Histogram<Duration>,
}

#[vise::register]
pub(crate) static POLICY_CLIENT_METRICS: vise::Global<PolicyClientMetrics> = vise::Global::new();
