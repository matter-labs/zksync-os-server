//! Metrics for the object storage.

use std::time::Duration;

use vise::{Buckets, Counter, Histogram, LabeledFamily, LatencyObserver, Metrics};

use crate::Bucket;

const BYTES_BUCKETS: Buckets = Buckets::exponential(1.0..=16.0 * 1_024.0 * 1_024.0, 8.0);

#[derive(Debug, Metrics)]
#[metrics(prefix = "server_object_store")]
pub(crate) struct ObjectStoreMetrics {
    /// Latency to fetch an object from the store (accounting for retries).
    #[metrics(buckets = Buckets::LATENCIES, labels = ["bucket"])]
    fetching_time: LabeledFamily<&'static str, Histogram<Duration>>,
    /// Latency to store an object in the store (accounting for retries).
    #[metrics(buckets = Buckets::LATENCIES, labels = ["bucket"])]
    storing_time: LabeledFamily<&'static str, Histogram<Duration>>,
    /// Size of the payloads stored in the object store.
    #[metrics(buckets = BYTES_BUCKETS, labels = ["bucket"])]
    pub payload_size: LabeledFamily<&'static str, Histogram<usize>>,
    /// Total number of bytes read from the object store.
    pub storage_read_total_bytes: Counter,
    /// Number of read/write operations performed, labeled by operation type (`read` or `write`).
    #[metrics(labels = ["op"])]
    pub read_write_ops: LabeledFamily<&'static str, Counter>,
}

impl ObjectStoreMetrics {
    pub fn start_fetch(&self, bucket: Bucket) -> LatencyObserver<'_> {
        self.fetching_time[&bucket.0].start()
    }

    pub fn start_store(&self, bucket: Bucket) -> LatencyObserver<'_> {
        self.storing_time[&bucket.0].start()
    }
}

#[vise::register]
pub(crate) static OBJECT_STORE_METRICS: vise::Global<ObjectStoreMetrics> = vise::Global::new();
