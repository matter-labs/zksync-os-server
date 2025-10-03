use crate::peekable_receiver::PeekableReceiver;
use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc;

/// A component that generates messages and starts the pipeline.
/// Examples: command_source, block replay streams
/// ONLY IMPLEMENT THIS TRAIT FOR MARKER STRUCTS (no fields in struct).
#[async_trait]
pub trait Source {
    /// The type of messages this source produces
    type Output: Send + 'static;

    /// Parameters needed to initialize this source
    type Params: Send + 'static;

    /// Human-readable name for logging and metrics
    const NAME: &'static str;

    /// Buffer size for the output channel
    const OUTPUT_BUFFER_SIZE: usize;

    /// Run the source component, sending messages to the output
    async fn run(
        params: Self::Params,
        output: mpsc::Sender<Self::Output>,
    ) -> Result<()>;
}

/// A component that transforms messages in the pipeline.
/// Examples: ProverInputGenerator, Batcher, L1 senders
///
/// ONLY IMPLEMENT THIS TRAIT FOR MARKER STRUCTS (no fields in struct).
/// Any state is confined to the ` run () ` method.
#[async_trait]
pub trait PipelineComponent {
    /// The type of messages this component receives
    type Input: Send + 'static;

    /// The type of messages this component produces
    type Output: Send + 'static;

    /// Parameters needed to initialize this component
    type Params: Send + 'static;

    /// Human-readable name for logging and metrics
    const NAME: &'static str;

    /// Buffer size for the output channel
    const OUTPUT_BUFFER_SIZE: usize;

    /// Run the component, receiving from input and sending to output
    async fn run(
        params: Self::Params,
        input: PeekableReceiver<Self::Input>,
        output: mpsc::Sender<Self::Output>,
    ) -> Result<()>;
}

/// A component that consumes messages and ends the pipeline.
/// Examples: BatchSink, logging components
/// ONLY IMPLEMENT THIS TRAIT FOR MARKER STRUCTS (no fields in struct).
#[async_trait]
pub trait Sink {
    /// The type of messages this sink receives
    type Input: Send + 'static;

    /// Parameters needed to initialize this sink
    type Params: Send + 'static;

    /// Human-readable name for logging and metrics
    const NAME: &'static str;

    /// Run the sink component, receiving messages from the input
    async fn run(
        params: Self::Params,
        input: PeekableReceiver<Self::Input>,
    ) -> Result<()>;
}