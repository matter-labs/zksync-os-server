use vise::{Counter, EncodeLabelSet, EncodeLabelValue, Gauge, Metrics, MetricsFamily, Unit};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EncodeLabelValue, EncodeLabelSet)]
#[metrics(label = "chain_id")]
pub(crate) struct LogCacheLabels(pub u64);

impl std::fmt::Display for LogCacheLabels {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Metrics)]
pub(crate) struct LogCacheMetrics {
    pub hits: Counter,
    pub fallbacks: Counter,
    pub blocks_loaded: Counter,
    #[metrics(unit = Unit::Bytes)]
    pub approx_memory: Gauge<usize>,
}

#[vise::register]
pub(crate) static METRICS: MetricsFamily<LogCacheLabels, LogCacheMetrics> = MetricsFamily::new();
