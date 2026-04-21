use crate::component_id::ComponentId;
use crate::peekable_receiver::PeekableReceiver;
use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc;
use zksync_os_observability::ComponentStateReporter;

/// A component that transforms messages in the pipeline.
/// Examples: ProverInputGenerator, Batcher, L1 senders
///
/// Components construct themselves with all needed parameters, then get consumed by `run()`.
#[async_trait]
pub trait PipelineComponent: Send + 'static {
    /// The type of messages this component receives
    type Input: Send + 'static;

    /// The type of messages this component produces
    type Output: Send + 'static;

    /// Identity of this pipeline component.
    /// Used as the task name for logging, shutdown tracking, and state-monitor adjacency.
    const COMPONENT_ID: ComponentId;

    /// Whether `Pipeline::pipe()` should register this component with the state monitor.
    /// Sources/sinks that never call `record_*` (e.g. `NoOpSink`, command sources) opt out
    /// by setting this to `false`; doing so also keeps `prev_id` anchored to the previous
    /// monitored component, so the next `.pipe()` edge skips over this one.
    const REGISTER_WITH_MONITOR: bool = true;

    /// Run the component, receiving from input and sending to output. `state_reporter` is the
    /// reporter `pipe()` created for this component; unmonitored components (those with
    /// `REGISTER_WITH_MONITOR = false`) receive a detached reporter that is never watched.
    async fn run(
        self,
        input: PeekableReceiver<Self::Input>,
        output: mpsc::UnboundedSender<Self::Output>,
        state_reporter: ComponentStateReporter,
    ) -> Result<()>;
}
