use vise::{Counter, Metrics};

/// These metrics are exported with push exporter.
/// The `PushMetrics` suffix is required for this to work.
///
/// Use case: report something right before stopping the node, without waiting for a scrape,
/// so these metrics can be used for alerts.
///
/// Be careful: There can be some differences in how the metrics are handled by Prometheus compared
/// to pull exporter.
#[derive(Debug, Metrics)]
pub struct RevmCheckerPushMetrics {
    /// Number of REVM divergences detected during this run. Used for alerts.
    pub revm_divergences_detected: Counter,
}

#[vise::register]
pub static PUSH_METRICS: vise::Global<RevmCheckerPushMetrics> = vise::Global::new();
