use async_trait::async_trait;
use std::time::Duration;
use tokio::sync::mpsc;
use zksync_os_batch_types::batcher_model::{FriProof, SignedBatchEnvelope};
use zksync_os_internal_config::InternalConfigManager;
use zksync_os_observability::ComponentStateReporter;
use zksync_os_pipeline::{PeekableReceiver, PipelineComponent};
use zksync_os_storage_api::ReadFinality;

/// Final destination for all processed batches
// todo: add metrics
pub struct BatchSink {
    internal_config_manager: InternalConfigManager,
}

impl BatchSink {
    pub fn new(internal_config_manager: InternalConfigManager) -> Self {
        Self {
            internal_config_manager,
        }
    }
}

#[async_trait]
impl PipelineComponent for BatchSink {
    type Input = SignedBatchEnvelope<FriProof>;
    type Output = ();

    const COMPONENT_ID: zksync_os_pipeline::ComponentId =
        zksync_os_pipeline::ComponentId::BatchSink;

    async fn run(
        self,
        mut input: PeekableReceiver<Self::Input>,
        _output: mpsc::Sender<Self::Output>,
        state_reporter: ComponentStateReporter,
    ) -> anyhow::Result<()> {
        let mut internal_config = self.internal_config_manager.read_config()?;
        loop {
            let Some(envelope) = input.recv_and_record_picked(&state_reporter).await else {
                tracing::info!("inbound channel closed");
                return Ok(());
            };
            tracing::info!(
                batch_number = envelope.batch_number(),
                latency_tracker = %envelope.latency_tracker,
                tx_count = envelope.batch.tx_count,
                block_from = envelope.batch.first_block_number,
                block_to = envelope.batch.last_block_number,
                proof = ?envelope.data,
                " ▶▶▶ Batch has been fully processed"
            );
            state_reporter.record_processed(
                envelope.batch.last_block_number,
                None,
                Some(envelope.batch_number()),
            );
            if let Some(n) = internal_config.failing_block
                && envelope.batch.last_block_number >= n
            {
                let message = "Removing `failing_block` from the internal config";
                tracing::info!(message);
                internal_config.failing_block = None;
                internal_config.failing_block_hash = None;
                self.internal_config_manager
                    .write_config_and_panic(&internal_config, message)?;
            }
        }
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

    const COMPONENT_ID: zksync_os_pipeline::ComponentId = zksync_os_pipeline::ComponentId::NoopSink;

    async fn run(
        self,
        mut input: PeekableReceiver<Self::Input>,
        _output: mpsc::Sender<Self::Output>,
        _state_reporter: ComponentStateReporter,
    ) -> anyhow::Result<()> {
        while input.recv().await.is_some() {
            // No-op: just receive and discard
        }
        anyhow::bail!("Input channel closed");
    }
}

/// Task that periodically checks the finality status and removes `failing_block` from the internal config
/// when the specified block number is reached. Should only be run for ENs.
pub async fn clear_failing_block_config_task<F: ReadFinality>(
    finality: F,
    internal_config_manager: InternalConfigManager,
) {
    let mut internal_config = internal_config_manager
        .read_config()
        .expect("failed to read internal config");
    if let Some(failing_block_number) = internal_config.failing_block {
        tracing::info!(
            "Starting `clear_failing_block_config_task` to monitor finality status for block number {failing_block_number}"
        );
        loop {
            if finality.get_finality_status().last_executed_block >= failing_block_number {
                let message = "Removing `failing_block` from the internal config";
                tracing::info!(message);
                internal_config.failing_block = None;
                internal_config.failing_block_hash = None;
                internal_config_manager
                    .write_config_and_panic(&internal_config, message)
                    .expect("failed to write internal config");
            } else {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    } else {
        // Do nothing
    }
}
