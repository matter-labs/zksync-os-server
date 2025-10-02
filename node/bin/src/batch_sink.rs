use tokio::sync::mpsc::Receiver;
use zksync_os_l1_sender::batcher_model::{FriProof, SignedBatchEnvelope};

/// Final destination for all processed batches
/// Only used for metrics, logging and analytics.
// todo: add metrics
pub struct BatchSink {
    // == plumbing ==
    // inbound
    committed_batch_receiver: Receiver<SignedBatchEnvelope<FriProof>>,
}

impl BatchSink {
    // todo: no need to pass FriProof here, just SignedBatchEnvelope
    pub fn new(committed_batch_receiver: Receiver<SignedBatchEnvelope<FriProof>>) -> Self {
        Self {
            committed_batch_receiver,
        }
    }

    pub async fn run(mut self) -> anyhow::Result<()> {
        while let Some(envelope) = self.committed_batch_receiver.recv().await {
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
