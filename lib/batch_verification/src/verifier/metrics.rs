use vise::{Counter, Gauge, Metrics};

#[derive(Debug, Metrics)]
#[metrics(prefix = "batch_verification_responder")]
pub struct BatchVerificationResponderMetrics {
    /// Number of blocks currently cached
    block_cache_size: Gauge<usize>,
    /// Lowest block number in the cache
    block_cache_from_number: Gauge<u64>,
    /// Highest block number in the cache
    block_cache_to_number: Gauge<u64>,
    /// Last request ID processed
    last_request_id: Gauge<u64>,
    /// Last batch number in last processed request id
    last_batch_number: Gauge<u64>,
    /// Total number of requests processed
    request_count: Counter,
    /// Number of refused responses
    request_failed_count: Counter,
    /// Number of successful responses / signed requests
    request_success_count: Counter,
}

#[vise::register]
pub(crate) static BATCH_VERIFICATION_RESPONDER_METRICS: vise::Global<
    BatchVerificationResponderMetrics,
> = vise::Global::new();

impl BatchVerificationResponderMetrics {
    pub fn update_cache_range(&self, from: u64, to: u64) {
        self.block_cache_from_number.set(from);
        self.block_cache_to_number.set(to);
        self.block_cache_size.set((to + 1 - from) as usize);
    }

    pub fn record_request_success(&self, request_id: u64, batch_number: u64) {
        self.request_count.inc();
        self.request_success_count.inc();
        self.last_request_id.set(request_id);
        self.last_batch_number.set(batch_number);
    }

    pub fn record_request_failure(&self, request_id: u64, batch_number: u64) {
        self.request_count.inc();
        self.request_failed_count.inc();
        self.last_request_id.set(request_id);
        self.last_batch_number.set(batch_number);
    }
}
