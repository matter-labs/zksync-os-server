use std::time::Duration;
use vise::{Buckets, Histogram, LabeledFamily, Metrics, Unit};

const LATENCIES: Buckets = Buckets::exponential(0.00001..=10.0, 10.0);
const BYTES: Buckets = Buckets::exponential(1.0..=128.0 * 1024.0 * 1024.0, 2.0);
const SECONDS_PER_BYTE: Buckets = Buckets::exponential(0.0000000001..=0.001, 10.0);

#[derive(Debug, Metrics)]
#[metrics(prefix = "replay_archive")]
pub(crate) struct ReplayArchiveMetrics {
    #[metrics(unit = Unit::Seconds, buckets = LATENCIES)]
    pub gate_wait: Histogram<Duration>,

    #[metrics(unit = Unit::Bytes, labels = ["stage"], buckets = BYTES)]
    pub object_bytes: LabeledFamily<&'static str, Histogram<usize>>,

    #[metrics(unit = Unit::Seconds, buckets = LATENCIES)]
    pub encryption_time: Histogram<Duration>,

    #[metrics(unit = Unit::Seconds, buckets = SECONDS_PER_BYTE)]
    pub encryption_time_per_byte: Histogram<f64>,
}

#[vise::register]
pub(crate) static REPLAY_ARCHIVE_METRICS: vise::Global<ReplayArchiveMetrics> = vise::Global::new();
