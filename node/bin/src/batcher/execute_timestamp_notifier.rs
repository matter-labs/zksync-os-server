use async_trait::async_trait;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, watch};
use zksync_os_l1_sender::batcher_model::{FriProof, SignedBatchEnvelope};
use zksync_os_pipeline::{PeekableReceiver, PipelineComponent};

/// Pipeline step placed after the execute L1 sender.
///
/// For each batch that passes through (i.e., has been successfully executed on L1),
/// it notifies the batcher of the current wall-clock timestamp via a `watch` channel.
/// The batcher uses this timestamp to compute absolute batch deadlines.
pub struct ExecuteTimestampNotifier {
    pub sender: watch::Sender<Option<u64>>,
}

#[async_trait]
impl PipelineComponent for ExecuteTimestampNotifier {
    type Input = SignedBatchEnvelope<FriProof>;
    type Output = SignedBatchEnvelope<FriProof>;

    const NAME: &'static str = "execute_timestamp_notifier";
    const OUTPUT_BUFFER_SIZE: usize = 1;

    async fn run(
        self,
        input: PeekableReceiver<Self::Input>,
        output: mpsc::Sender<Self::Output>,
    ) -> anyhow::Result<()> {
        let mut input = input.into_inner();
        loop {
            let Some(envelope) = input.recv().await else {
                tracing::info!("inbound channel closed");
                return Ok(());
            };

            let now_unix = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock before UNIX epoch")
                .as_secs();

            tracing::debug!(
                batch_number = envelope.batch_number(),
                execute_timestamp = now_unix,
                "notifying batcher of execute timestamp"
            );
            // Ignore send errors — the batcher may have already shut down.
            let _ = self.sender.send(Some(now_unix));

            if output.send(envelope).await.is_err() {
                tracing::info!("outbound channel closed");
                return Ok(());
            }
        }
    }
}
