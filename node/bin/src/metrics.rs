use vise::{Gauge, LabeledFamily, Metrics};

#[derive(Debug, Metrics)]
#[metrics(prefix = "node_meta")]
pub struct NodeMetaMetrics {
    /// Gauge always set to `1` with `version` label set to the current semver version
    #[metrics(labels = ["version"])]
    pub version: LabeledFamily<&'static str, Gauge<u64>>,

    /// Gauge always set to `1` with `role` label set to "external_node" and "main_node"
    #[metrics(labels = ["role"])]
    pub role: LabeledFamily<&'static str, Gauge<u64>>,
}

#[vise::register]
pub(crate) static NODE_META_METRICS: vise::Global<NodeMetaMetrics> = vise::Global::new();
