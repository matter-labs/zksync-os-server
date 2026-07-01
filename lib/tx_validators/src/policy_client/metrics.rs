use std::time::Duration;
use vise::{
    Buckets, Counter, EncodeLabelSet, EncodeLabelValue, Family, Histogram, Metrics, MetricsFamily,
    Unit,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EncodeLabelSet, EncodeLabelValue)]
#[metrics(label = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    Allow,
    Deny,
}

/// Cheap reason breakdown for errors. The goal is operator-legible buckets,
/// not a perfect 1:1 with `TransportError` variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EncodeLabelSet, EncodeLabelValue)]
#[metrics(label = "kind", rename_all = "snake_case")]
pub enum ErrorKind {
    Timeout,
    Connect,
    Http,
    Status,
    MalformedResponse,
    ProtocolVersionMismatch,
}

/// Which call site this `PolicyClient` instance is serving. Stamped on the
/// client at construction time so admit / judge metrics partition cleanly
/// between the RPC boundary and the sequencer's block-build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EncodeLabelSet, EncodeLabelValue)]
#[metrics(label = "component", rename_all = "snake_case")]
pub enum Component {
    Rpc,
    Sequencer,
}

#[derive(Debug, Metrics)]
#[metrics(prefix = "policy_client")]
pub struct PolicyClientMetrics {
    /// Count of admit decisions, broken down by allow / deny.
    pub admit_decisions: Family<Outcome, Counter>,

    /// Count of admit errors (treated as fail-closed by the client).
    pub admit_errors: Family<ErrorKind, Counter>,

    /// Count of admit calls bypassed via the `bypass_from` allowlist.
    pub admit_bypassed: Counter,

    /// Latency of the admit round trip.
    /// Buckets span sub-ms to ~1s to cover both healthy localhost/UDS and
    /// worst-case TCP under load.
    #[metrics(unit = Unit::Seconds, buckets = Buckets::exponential(0.0001..=1.0, 2.0))]
    pub admit_latency: Histogram<Duration>,

    /// Count of judge decisions, broken down by allow / deny.
    pub judge_decisions: Family<Outcome, Counter>,

    /// Count of judge errors (treated as fail-closed by the client).
    pub judge_errors: Family<ErrorKind, Counter>,

    /// Count of judge calls bypassed via the `bypass_from` allowlist.
    pub judge_bypassed: Counter,

    /// Latency of the judge round trip. Same bucketing rationale as
    /// `admit_latency` — judge runs in the same hot path during block build.
    #[metrics(unit = Unit::Seconds, buckets = Buckets::exponential(0.0001..=1.0, 2.0))]
    pub judge_latency: Histogram<Duration>,
}

#[vise::register]
pub(crate) static POLICY_CLIENT_METRICS: MetricsFamily<Component, PolicyClientMetrics> =
    MetricsFamily::new();
