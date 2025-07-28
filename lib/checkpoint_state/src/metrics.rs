use std::time::Duration;
use vise::{Buckets, Histogram, LabeledFamily, Metrics, Unit};

const LATENCIES_FAST: Buckets = Buckets::exponential(0.0000001..=1.0, 2.0);
const BLOCKS_SCANNED: Buckets = Buckets::linear(1.0..=1000.0, 100.0);

#[derive(Debug, Metrics)]
#[metrics(prefix = "storage_view")]
pub struct StorageViewMetrics {
    #[metrics(unit = Unit::Seconds, labels = ["stage"], buckets = LATENCIES_FAST)]
    pub access: LabeledFamily<&'static str, Histogram<Duration>>,

    #[metrics(buckets = BLOCKS_SCANNED)]
    pub diffs_scanned: Histogram<u64>,
}

#[derive(Debug, Metrics)]
#[metrics(prefix = "storage_write")]
pub struct StorageWriteMetrics {
    #[metrics(unit = Unit::Seconds, buckets = LATENCIES_FAST)]
    pub compact: Histogram<Duration>,

    #[metrics(buckets = Buckets::exponential(1.0..=1000.0, 2.0))]
    pub compact_batch_size: Histogram<u64>,

    #[metrics(unit = Unit::Seconds, buckets = LATENCIES_FAST)]
    pub add_diff: Histogram<Duration>,
}

#[derive(Debug, Metrics)]
#[metrics(prefix = "preimages")]
pub struct PreimagesMetrics {
    #[metrics(unit = Unit::Seconds, labels = ["stage"], buckets = LATENCIES_FAST)]
    pub get: LabeledFamily<&'static str, Histogram<Duration>>,

    #[metrics(unit = Unit::Seconds, labels = ["stage"], buckets = LATENCIES_FAST)]
    pub set: LabeledFamily<&'static str, Histogram<Duration>>,
}

#[vise::register]
pub(crate) static STORAGE_VIEW_METRICS: vise::Global<StorageViewMetrics> = vise::Global::new();
#[vise::register]
pub(crate) static STORAGE_MAP_METRICS: vise::Global<StorageWriteMetrics> = vise::Global::new();
#[vise::register]
pub(crate) static PREIMAGES_METRICS: vise::Global<PreimagesMetrics> = vise::Global::new();
