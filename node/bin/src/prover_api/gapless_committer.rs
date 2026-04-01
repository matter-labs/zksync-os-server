use crate::prover_api::proof_storage::{ProofStorage, StoredBatch};
use anyhow::Context;
use async_trait::async_trait;
use std::collections::BTreeMap;
use zksync_os_contract_interface::l1_discovery::BatchVerificationSL;
use zksync_os_l1_sender::batcher_metrics::BatchExecutionStage;
use zksync_os_l1_sender::batcher_model::{FriProof, SignedBatchEnvelope};
use zksync_os_l1_sender::commands::L1SenderCommand;
use zksync_os_l1_sender::commands::commit::CommitCommand;
use zksync_os_observability::{ComponentHealthReporter, GenericComponentState};
use zksync_os_pipeline::{PipelineComponent, TrackedUnboundedReceiver, TrackedUnboundedSender};

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
    pub health_reporter: ComponentHealthReporter,
}

#[async_trait]
impl PipelineComponent for GaplessCommitter {
    type Input = SignedBatchEnvelope<FriProof>;
    type Output = L1SenderCommand<CommitCommand>;

    const COMPONENT_ID: zksync_os_pipeline::ComponentId =
        zksync_os_pipeline::ComponentId::GaplessCommitter;

    async fn run(
        self,
        mut input: TrackedUnboundedReceiver<Self::Input>,
        output: TrackedUnboundedSender<Self::Output>,
    ) -> anyhow::Result<()> {
        let health_reporter = self.health_reporter;

        let mut buffer: BTreeMap<u64, SignedBatchEnvelope<FriProof>> = BTreeMap::new();
        let mut next_expected_batch_number = self.next_expected_batch_number;

        loop {
            health_reporter.enter_state(GenericComponentState::Idle);
            // Plain recv: do NOT record health on arrival. A batch sitting in the
            // reorder buffer has not been committed; recording it here would report
            // a position ahead of what has actually been processed.
            let Some(batch) = input.recv().await else {
                tracing::info!("inbound channel closed");
                return Ok(());
            };
            health_reporter.enter_state(GenericComponentState::Active);
            buffer.insert(batch.batch_number(), batch);

            // Flush ready batches in order.
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
                    // Save the position info before consuming the batch.
                    let last_block_number = batch.batch.last_block_number;
                    let last_block_timestamp = batch.batch.batch_info.last_block_timestamp;
                    let batch = batch.with_stage(BatchExecutionStage::FriProofStored);
                    let stored_batch = StoredBatch::V1(batch);
                    self.proof_storage
                        .save_batch_with_proof(&stored_batch)
                        .await?;
                    let result = if stored_batch.batch_number() <= self.last_committed_batch_number
                    {
                        L1SenderCommand::Passthrough(Box::new(stored_batch.batch_envelope()))
                    } else {
                        CommitCommand::try_new(
                            &self.batch_verification_l1_config,
                            stored_batch.batch_envelope(),
                        )
                        .map(L1SenderCommand::SendToL1)
                        .context("Committer batch signature failure")?
                    };
                    output
                        .send(result)
                        .ok()
                        .context("outbound channel closed")?;
                    // Record health only after the batch has been committed and sent downstream.
                    health_reporter.record_processed(last_block_number, Some(last_block_timestamp));
                }
            }
        }
    }
}
