use vise::{Counter, EncodeLabelSet, EncodeLabelValue, Gauge, Metrics, MetricsFamily, Unit};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EncodeLabelValue, EncodeLabelSet)]
#[metrics(label = "chain_id")]
pub(crate) struct LogsCacheLabels(pub u64);

impl std::fmt::Display for LogsCacheLabels {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

// Use provider prefix here, because it should be eventually merged into provider metrics.
#[derive(Debug, Metrics)]
#[metrics(prefix = "provider_log_cache")]
pub(crate) struct LogsCacheMetrics {
    pub hits: Counter,
    pub fallbacks: Counter,
    pub blocks_loaded: Counter,
    #[metrics(unit = Unit::Bytes)]
    pub approx_memory: Gauge<usize>,
}

#[vise::register]
pub(crate) static METRICS: MetricsFamily<LogsCacheLabels, LogsCacheMetrics> = MetricsFamily::new();
