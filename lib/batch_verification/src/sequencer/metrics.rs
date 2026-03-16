use std::time::Duration;
use vise::{Buckets, Counter, Gauge, Histogram, LabeledFamily, Metrics, Unit};

#[derive(Debug, Metrics)]
#[metrics(prefix = "batch_verification_sequencer")]
pub struct BatchVerificationSequencerMetrics {
    /// Threshold chosen by the main node for batch verification.
    pub threshold: Gauge<u64>,

    /// Number of signer addresses chosen by the main node for batch verification.
    pub validators_count: Gauge<usize>,

    /// Histogram of the attempt number on which batch verification succeeds. factor 1.3 -> 19 buckets
    #[metrics(buckets = Buckets::exponential(1.0..=128.0, 1.3))]
    pub attempts_to_success: Histogram<u64>,

    /// Total latency to collect enough signatures for a batch (including retries)
    #[metrics(unit = Unit::Seconds, buckets = Buckets::LATENCIES)]
    pub total_latency: Histogram<Duration>,

    /// Latency from request to Success responses from each signer, labeled by signer key address.
    #[metrics(unit = Unit::Seconds, buckets = Buckets::LATENCIES, labels = ["signer"])]
    pub per_signer_latency: LabeledFamily<String, Histogram<Duration>>,

    /// Histogram of successful signing per attempt number and signer. Note that if
    /// retries where needed, all successes will be recorded.
    #[metrics(labels = ["signer"], buckets = Buckets::exponential(1.0..=128.0, 1.3))]
    pub successful_attempt_per_signer: LabeledFamily<String, Histogram<u64>>,

    /// Total number of responses received by the main node that did not validate into
    /// a usable signature set entry (e.g. refused, invalid, unknown signer).
    #[metrics(labels = ["reason"])]
    pub failed_responses: LabeledFamily<&'static str, Counter>,

    /// Latest request_id used for batch verification
    pub last_request_id: Gauge<u64>,

    /// Latest batch_number for which we sent a signing request
    pub last_batch_number: Gauge<u64>,
}

#[vise::register]
pub(crate) static BATCH_VERIFICATION_SEQUENCER_METRICS: vise::Global<
    BatchVerificationSequencerMetrics,
> = vise::Global::new();
