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

    /// Human-readable name for logging and metrics
    const NAME: &'static str;

    /// Buffer size for the output channel
    const OUTPUT_BUFFER_SIZE: usize;

    /// Run the component, receiving from input and sending to output.
    async fn run(
        self,
        input: PeekableReceiver<Self::Input>,
        output: mpsc::Sender<Self::Output>,
    ) -> Result<()>;
}
