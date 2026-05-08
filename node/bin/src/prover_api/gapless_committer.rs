use crate::prover_api::proof_storage::{ProofStorage, StoredBatch};
use anyhow::Context;
use async_trait::async_trait;
use std::collections::BTreeMap;
use tokio::sync::mpsc;
use zksync_os_batch_types::batcher_model::{FriProof, SignedBatchEnvelope};
use zksync_os_batcher_metrics::BatchExecutionStage;
use zksync_os_contract_interface::l1_discovery::BatchVerificationSL;
use zksync_os_l1_sender::commands::L1SenderCommand;
use zksync_os_l1_sender::commands::commit::CommitCommand;
use zksync_os_observability::{ComponentStateReporter, GenericComponentState};
use zksync_os_pipeline::{PeekableReceiver, PipelineComponent, SendAndRecordExt};

/// Receives Batches with proofs - potentially out of order;
/// * Fixes the order (by filling in the `buffer` field);
/// * Saves to the `proof_storage`
/// * Sends downstream:
///    * For already committed batches: `L1SenderCommand::Passthrough`
///    * For batches that are not yet committed: `L1SenderCommand::SendToL1`
///
pub struct GaplessCommitter {
    pub next_expected_batch_number: u64,
    pub last_committed_batch_number: u64,
    pub proof_storage: ProofStorage,
    pub batch_verification_l1_config: BatchVerificationSL,
}

#[async_trait]
impl PipelineComponent for GaplessCommitter {
    type Input = SignedBatchEnvelope<FriProof>;
    type Output = L1SenderCommand<CommitCommand>;

    const COMPONENT_ID: zksync_os_pipeline::ComponentId =
        zksync_os_pipeline::ComponentId::GaplessCommitter;

    async fn run(
        self,
        mut input: PeekableReceiver<Self::Input>,
        output: mpsc::Sender<Self::Output>,
        state_reporter: ComponentStateReporter,
    ) -> anyhow::Result<()> {
        let mut buffer: BTreeMap<u64, SignedBatchEnvelope<FriProof>> = BTreeMap::new();
        let mut next_expected_batch_number = self.next_expected_batch_number;

        loop {
            state_reporter.enter_state(GenericComponentState::Idle);
            match input.recv_and_record_picked(&state_reporter).await {
                Some(batch) => {
                    state_reporter.enter_state(GenericComponentState::Active);
                    buffer.insert(batch.batch_number(), batch);

                    // Flush ready batches
                    let mut ready: Vec<SignedBatchEnvelope<FriProof>> = Vec::new();
                    while let Some(next_batch) = buffer.remove(&next_expected_batch_number) {
                        ready.push(next_batch);
                        next_expected_batch_number += 1;
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
                            let result = if stored_batch.batch_number()
                                <= self.last_committed_batch_number
                            {
                                L1SenderCommand::Passthrough(Box::new(
                                    stored_batch.batch_envelope(),
                                ))
                            } else {
                                CommitCommand::try_new(
                                    &self.batch_verification_l1_config,
                                    stored_batch.batch_envelope(),
                                )
                                .map(L1SenderCommand::SendToL1)
                                .context("Committer batch signature failure")?
                            };
                            output.send_and_record(result, &state_reporter)?;
                        }
                    }
                }
                None => {
                    tracing::info!("inbound channel closed");
                    return Ok(());
                }
            }
        }
    }
}
