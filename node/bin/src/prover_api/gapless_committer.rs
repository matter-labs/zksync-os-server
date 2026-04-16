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
        let mut buffer: BTreeMap<u64, SignedBatchEnvelope<FriProof>> = BTreeMap::new();
        let mut next_expected_batch_number = self.next_expected_batch_number;

        loop {
            self.health_reporter
                .enter_state(GenericComponentState::Idle);
            // Plain recv: record_picked on arrival (tracks channel dequeue time), but
            // do NOT call record_processed here. A batch sitting in the reorder buffer
            // has not been committed; record_processed fires only after send_and_record.
            let Some(batch) = input.recv().await else {
                tracing::info!("inbound channel closed");
                return Ok(());
            };
            let arrived_batch_number = batch.batch_number();
            let arrived_last_block = batch.batch.last_block_number;
            self.health_reporter.record_picked(
                arrived_last_block,
                Some(batch.batch.batch_info.last_block_timestamp),
            );
            self.health_reporter
                .record_batch_picked(arrived_batch_number);
            self.health_reporter
                .enter_state(GenericComponentState::Active);
            buffer.insert(arrived_batch_number, batch);

            if arrived_batch_number != next_expected_batch_number {
                let buffer_size = buffer.len();
                tracing::debug!(
                    "GaplessCommitter: out-of-order batch {arrived_batch_number} buffered (last_block={arrived_last_block}), waiting for batch {next_expected_batch_number}, buffer_size={buffer_size}"
                );
            }

            // Flush ready batches in order.
            let mut ready: Vec<SignedBatchEnvelope<FriProof>> = Vec::new();
            while let Some(next_batch) = buffer.remove(&next_expected_batch_number) {
                ready.push(next_batch);
                next_expected_batch_number += 1;
            }

            if !ready.is_empty() {
                tracing::info!(
                    "GaplessCommitter: saving {} batches {}-{} to proof_storage, buffer_size={}",
                    ready.len(),
                    ready[0].batch_number(),
                    ready.last().unwrap().batch_number(),
                    buffer.len(),
                );
                for batch in ready {
                    let batch = batch.with_stage(BatchExecutionStage::FriProofStored);
                    let stored_batch = StoredBatch::V1(batch);
                    self.proof_storage
                        .save_batch_with_proof(&stored_batch)
                        .await?;
                    let batch_num = stored_batch.batch_number();
                    let result = if batch_num <= self.last_committed_batch_number {
                        L1SenderCommand::Passthrough(Box::new(stored_batch.batch_envelope()))
                    } else {
                        CommitCommand::try_new(
                            &self.batch_verification_l1_config,
                            stored_batch.batch_envelope(),
                        )
                        .map(L1SenderCommand::SendToL1)
                        .context("Committer batch signature failure")?
                    };
                    // Record health only after the batch has been committed and sent downstream.
                    if output
                        .send_and_record(result, &self.health_reporter)
                        .is_err()
                    {
                        anyhow::bail!("Outbound channel closed");
                    }
                    self.health_reporter.record_batch_number(batch_num);
                }
            }
        }
    }
}
