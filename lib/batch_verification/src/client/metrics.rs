use vise::{Gauge, Metrics};

#[derive(Debug, Metrics)]
#[metrics(prefix = "batch_verification_client")]
pub struct BatchVerificationClientMetrics {
    pub block_cache_size: Gauge<usize>,
}

#[vise::register]
pub(crate) static BATCH_VERIFICATION_CLIENT_METRICS: vise::Global<BatchVerificationClientMetrics> =
    vise::Global::new();
