use async_trait::async_trait;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use zksync_os_config_db::ConfigDB;
use zksync_os_l1_sender::batcher_model::{FriProof, SignedBatchEnvelope};
use zksync_os_pipeline::{PeekableReceiver, PipelineComponent};
use zksync_os_storage_api::ReadFinality;

/// Final destination for all processed batches
// todo: add metrics
pub struct BatchSink {
    clear_config_after_block_number: Option<u64>,
    config_db: Arc<Mutex<ConfigDB>>,
}

impl BatchSink {
    pub fn new(
        clear_config_after_block_number: Option<u64>,
        config_db: Arc<Mutex<ConfigDB>>,
    ) -> Self {
        Self {
            clear_config_after_block_number,
            config_db,
        }
    }
}

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
            if let Some(n) = self.clear_config_after_block_number
                && envelope.batch.last_block_number >= n
            {
                tracing::info!("Clearing config DB and restarting node");
                let db = self.config_db.lock().unwrap();
                db.delete()?;
                panic!("Restarting node to apply new configuration");
            }
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

/// Task that periodically checks the finality status and clears the config DB when the specified block number is reached.
/// Should only be run for ENs.
pub async fn clear_db_config_task<F: ReadFinality>(
    clear_config_after_block_number: u64,
    finality: F,
    config_db: Arc<Mutex<ConfigDB>>,
) -> anyhow::Result<()> {
    loop {
        if finality.get_finality_status().last_executed_block >= clear_config_after_block_number {
            tracing::info!("Clearing config DB and restarting node");
            let db = config_db.lock().unwrap();
            db.delete()?;
            panic!("Restarting node to apply new configuration");
        } else {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
}
