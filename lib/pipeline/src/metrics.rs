use vise::{Counter, LabeledFamily, Metrics};

#[derive(Debug, Metrics)]
#[metrics(prefix = "pipeline")]
pub struct PipelineMetrics {
    /// Number of times a component's output channel was full and the send had to wait.
    /// Labeled by the producer component name.
    #[metrics(labels = ["component"])]
    pub channel_stall_count: LabeledFamily<&'static str, Counter<u64>>,
}

#[vise::register]
pub static PIPELINE_METRICS: vise::Global<PipelineMetrics> = vise::Global::new();
