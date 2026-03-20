use crate::config::ComponentId;
use vise::{EncodeLabelSet, Family, Gauge, Metrics};

#[derive(Debug, Clone, PartialEq, Eq, Hash, EncodeLabelSet)]
pub struct ComponentLabel {
    pub component: &'static str,
}

impl From<ComponentId> for ComponentLabel {
    fn from(id: ComponentId) -> Self {
        Self {
            component: id.as_str(),
        }
    }
}

#[derive(Debug, Metrics)]
#[metrics(prefix = "pipeline")]
pub struct MonitorMetrics {
    /// 1 if this component is currently an active backpressure cause, else 0.
    pub backpressure_active: Family<ComponentLabel, Gauge<u64>>,
    /// Blocks behind pipeline head. 0 when component is idle (WaitingRecv/Processing).
    pub component_block_lag: Family<ComponentLabel, Gauge<u64>>,
    /// Seconds the component has been in WaitingSend. 0 when not in WaitingSend.
    pub component_waiting_send_seconds: Family<ComponentLabel, Gauge<f64>>,
}

#[vise::register]
pub static MONITOR_METRICS: vise::Global<MonitorMetrics> = vise::Global::new();
