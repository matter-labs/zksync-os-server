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

    /// Id of this component.
    const COMPONENT_ID: ComponentId;

    /// Capacity of the output channel for this component.
    const OUTPUT_CHANNEL_CAPACITY: usize = 4096;

    /// Run the component, receiving from input and sending to output.
    async fn run(
        self,
        input: PeekableReceiver<Self::Input>,
        output: mpsc::Sender<Self::Output>,
        state_reporter: ComponentStateReporter,
    ) -> Result<()>;
}
