use tokio::sync::mpsc::{Receiver, Sender};
use zksync_os_l1_sender::batcher_model::{FriProof, SignedBatchEnvelope};
use zksync_os_l1_sender::commands::L1SenderCommand;

/// Forwards all commands from `inbound` to `outbound` without actually sending them to L1.
/// Useful for debugging.
/// Note: l1-related `BatchExecutionStage`s are absent for such batches' `LatencyTracker`.
pub async fn run_noop_l1_sender<Input: L1SenderCommand>(
    mut inbound: Receiver<Input>,
    outbound: Sender<SignedBatchEnvelope<FriProof>>,
) -> anyhow::Result<()> {
    loop {
        match inbound.recv().await {
            Some(batch) => {
                tracing::info!(
                    batch = %batch,
                    command = Input::NAME,
                    "Skipping batch L1 operation"
                );
                for output_envelope in batch.into() {
                    outbound.send(output_envelope).await?;
                }
            }
            None => {
                anyhow::bail!("channel unexpectedly closed");
            }
        }
    }
}
