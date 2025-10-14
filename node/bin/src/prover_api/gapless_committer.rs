use crate::prover_api::proof_storage::{ProofStorage, StoredBatch};
use async_trait::async_trait;
use std::collections::BTreeMap;
use tokio::sync::mpsc;
use zksync_os_contract_interface::models::BatchDaInputMode;
use zksync_os_l1_sender::batcher_metrics::BatchExecutionStage;
use zksync_os_l1_sender::batcher_model::{BatchEnvelope, FriProof};
use zksync_os_l1_sender::commands::commit::CommitCommand;
use zksync_os_observability::{ComponentStateReporter, GenericComponentState};
use zksync_os_pipeline::{PeekableReceiver, PipelineComponent};

/// Receives Batches with proofs - potentially out of order;
/// Fixes the order (by filling in the `buffer` field);
/// Sends batches downstream:
///   * First to the `proof_storage`
///   * Then to the `l1sender_handle`
///
pub struct GaplessCommitter {
    pub next_expected: u64,
    pub proof_storage: ProofStorage,
    pub da_input_mode: BatchDaInputMode,
}

#[async_trait]
impl PipelineComponent for GaplessCommitter {
    type Input = BatchEnvelope<FriProof>;
    type Output = CommitCommand;

    const NAME: &'static str = "gapless_committer";
    const OUTPUT_BUFFER_SIZE: usize = 5;

    async fn run(
        self,
        mut input: PeekableReceiver<Self::Input>,
        output: mpsc::Sender<Self::Output>,
    ) -> anyhow::Result<()> {
        let latency_tracker = ComponentStateReporter::global()
            .handle_for("gapless_committer", GenericComponentState::WaitingRecv);

        let mut buffer: BTreeMap<u64, BatchEnvelope<FriProof>> = BTreeMap::new();
        let mut next_expected = self.next_expected;

        loop {
            latency_tracker.enter_state(GenericComponentState::WaitingRecv);
            match input.recv().await {
                Some(batch) => {
                    latency_tracker.enter_state(GenericComponentState::Processing);
                    buffer.insert(batch.batch_number(), batch);

                    // Flush ready batches
                    let mut ready: Vec<BatchEnvelope<FriProof>> = Vec::new();
                    while let Some(next_batch) = buffer.remove(&next_expected) {
                        ready.push(next_batch);
                        next_expected += 1;
                    }

                    if !ready.is_empty() {
                        tracing::info!(
                            buffer_size = buffer.len(),
                            "Saving {} (batches {}-{}) to proof_storage",
                            ready.len(),
                            ready[0].batch_number(),
                            ready.last().unwrap().batch_number()
                        );
                        for batch in ready {
                            let batch = batch.with_stage(BatchExecutionStage::FriProofStored);
                            let stored_batch = StoredBatch::V1(batch);
                            self.proof_storage
                                .save_batch_with_proof(&stored_batch)
                                .await?;
                            latency_tracker.enter_state(GenericComponentState::WaitingSend);
                            output
                                .send(CommitCommand::new(
                                    stored_batch.batch_envelope(),
                                    self.da_input_mode,
                                ))
                                .await?;
                            latency_tracker.enter_state(GenericComponentState::Processing);
                        }
                    }
                }
                None => {
                    anyhow::bail!("GaplessCommitter input stream ended unexpectedly");
                }
            }
        }
    }
}
