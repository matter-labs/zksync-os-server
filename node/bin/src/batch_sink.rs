use async_trait::async_trait;
use tokio::sync::mpsc;
use zksync_os_l1_sender::batcher_model::{FriProof, SignedBatchEnvelope};
use zksync_os_pipeline::{PeekableReceiver, PipelineComponent};

/// Final destination for all processed batches
/// Only used for metrics, logging and analytics.
// todo: add metrics
pub struct BatchSink;

#[async_trait]
impl PipelineComponent for BatchSink {
    type Input = SignedBatchEnvelope<FriProof>;
    type Output = ();

    const NAME: &'static str = "batch_sink";
    const OUTPUT_BUFFER_SIZE: usize = 1; // No output

    async fn run(
        self,
        input: PeekableReceiver<Self::Input>,
        _output: mpsc::Sender<Self::Output>,
    ) -> anyhow::Result<()> {
        let mut input = input.into_inner();
        while let Some(envelope) = input.recv().await {
            tracing::info!(
                batch_number = envelope.batch_number(),
                latency_tracker = %envelope.latency_tracker,
                tx_count = envelope.batch.tx_count,
                block_from = envelope.batch.first_block_number,
                block_to = envelope.batch.last_block_number,
                proof = ?envelope.data,
                " ▶▶▶ Batch has been fully processed"
            );
        }
        anyhow::bail!("Failed to receive committed batch");
    }
}

/// Generic no-op sink that receives and discards all input
/// Used for pipelines where the final component produces output that isn't needed
pub struct NoOpSink<T> {
    _phantom: std::marker::PhantomData<T>,
}

impl<T> NoOpSink<T> {
    pub fn new() -> Self {
        Self {
            _phantom: std::marker::PhantomData,
        }
    }
}

impl<T> Default for NoOpSink<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl<T: Send + 'static> PipelineComponent for NoOpSink<T> {
    type Input = T;
    type Output = ();

    const NAME: &'static str = "noop_sink";
    const OUTPUT_BUFFER_SIZE: usize = 1; // No output

    async fn run(
        self,
        input: PeekableReceiver<Self::Input>,
        _output: mpsc::Sender<Self::Output>,
    ) -> anyhow::Result<()> {
        let mut input = input.into_inner();
        while input.recv().await.is_some() {
            // No-op: just receive and discard
        }
        anyhow::bail!("Input channel closed");
    }
}
