use crate::component_id::ComponentId;
use crate::peekable_receiver::PeekableReceiver;
use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc;

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
    /// Used as the task name for logging, shutdown tracking, and health-monitor adjacency.
    const COMPONENT_ID: ComponentId;

    /// Run the component, receiving from input and sending to output.
    /// `output.send()` is synchronous and never blocks — the channel is unbounded.
    async fn run(
        self,
        input: PeekableReceiver<Self::Input>,
        output: mpsc::UnboundedSender<Self::Output>,
    ) -> Result<()>;
}
