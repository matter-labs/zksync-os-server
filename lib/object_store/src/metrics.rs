//! Metrics for the object storage.

use std::time::Duration;

use vise::{Buckets, Histogram, LabeledFamily, LatencyObserver, Metrics};

use crate::Bucket;

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
    #[metrics(buckets = Buckets::exponential(1.0..=16.0 * 1_024.0 * 1_024.0, 8.0), labels = ["bucket"])]
    pub payload_size: LabeledFamily<&'static str, Histogram<usize>>,
}

impl ObjectStoreMetrics {
    pub fn start_fetch(&self, bucket: Bucket) -> LatencyObserver<'_> {
        self.fetching_time[&bucket.as_str()].start()
    }

    pub fn start_store(&self, bucket: Bucket) -> LatencyObserver<'_> {
        self.storing_time[&bucket.as_str()].start()
    }
}

#[vise::register]
pub(crate) static OBJECT_STORE_METRICS: vise::Global<ObjectStoreMetrics> = vise::Global::new();
