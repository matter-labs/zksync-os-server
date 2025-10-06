use crate::batcher::seal_criteria::BatchInfoAccumulator;
use crate::config::BatcherConfig;
use crate::tree_manager::BlockMerkleTreeData;
use alloy::primitives::Address;
use anyhow::Context;
use async_trait::async_trait;
use std::pin::Pin;
use tokio::sync::mpsc;
use tokio::time::Sleep;
use tracing;
use zksync_os_contract_interface::models::StoredBatchInfo;
use zksync_os_interface::types::BlockOutput;
use zksync_os_l1_sender::batcher_metrics::BATCHER_METRICS;
use zksync_os_l1_sender::batcher_model::{
    BatchEnvelope, BatchForSigning, MissingSignature, ProverInput,
};
use zksync_os_merkle_tree::TreeBatchOutput;
use zksync_os_observability::{
    ComponentStateHandle, ComponentStateReporter, GenericComponentState,
};
use zksync_os_pipeline::{PeekableReceiver, PipelineComponent};
use zksync_os_storage_api::ReplayRecord;

pub mod batch_builder;
mod seal_criteria;
pub mod util;

/// Batcher component - handles batching logic, receives blocks and prepares batch data
#[derive(Debug)]
pub struct Batcher {
    pub chain_id: u64,
    pub chain_address: Address,
    pub first_block_to_process: u64,
    /// Last persisted block. We should not seal batches by timeout until this block is reached.
    /// This helps to avoid premature sealing due to timeout criterion, since for  every tick of the
    /// timer the `should_seal_by_timeout` call will return `true`
    pub last_persisted_block: u64,
    pub pubdata_limit_bytes: u64,
    pub batcher_config: BatcherConfig,
    pub prev_batch_info: StoredBatchInfo,
}

#[async_trait]
impl PipelineComponent for Batcher {
    type Input = (BlockOutput, ReplayRecord, ProverInput, BlockMerkleTreeData);
    type Output = BatchEnvelope<ProverInput, MissingSignature>;

    const NAME: &'static str = "batcher";
    const OUTPUT_BUFFER_SIZE: usize = 5;

    async fn run(
        mut self,
        mut input: PeekableReceiver<Self::Input>,
        output: mpsc::Sender<Self::Output>,
    ) -> anyhow::Result<()> {
        let latency_tracker = ComponentStateReporter::global()
            .handle_for("batcher", GenericComponentState::WaitingRecv);

        let mut first_block_in_batch = self.first_block_to_process;
        // Skip blocks that were already committed (we start the sequencer with replaying older blocks)
        // Normally, ProverInputGenerator would filter them out,
        // but with `prover_input_generator_force_process_old_blocks` config set to true, they end up here.
        while input
            .peek_recv(|(b, _, _, _)| b.header.number < first_block_in_batch)
            .await
            .context("channel closed while skipping already processed blocks")?
        {
            input.recv().await;
        }
        loop {
            let batch_envelope = self
                .create_batch(&mut input, &latency_tracker, first_block_in_batch)
                .await?;
            BATCHER_METRICS
                .transactions_per_batch
                .observe(batch_envelope.batch.tx_count as u64);

            tracing::info!(
                batch_number = batch_envelope.batch_number(),
                batch_metadata = ?batch_envelope.batch,
                block_count = batch_envelope.batch.last_block_number - batch_envelope.batch.first_block_number + 1,
                new_state_commitment = ?batch_envelope.batch.batch_info.new_state_commitment,
                "Batch created"
            );

            tracing::debug!(
                batch_number = batch_envelope.batch_number(),
                da_commitment = ?batch_envelope.batch.batch_info.operator_da_input,
                "Batch da_input",
            );

            first_block_in_batch = batch_envelope.batch.last_block_number + 1;
            latency_tracker.enter_state(GenericComponentState::WaitingSend);
            output
                .send(batch_envelope)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to send batch data: {e}"))?;
        }
    }
}

impl Batcher {
    async fn create_batch(
        &mut self,
        block_receiver: &mut PeekableReceiver<(
            BlockOutput,
            ReplayRecord,
            ProverInput,
            BlockMerkleTreeData,
        )>,
        latency_tracker: &ComponentStateHandle<GenericComponentState>,
        expected_first_block: u64,
    ) -> anyhow::Result<BatchForSigning<ProverInput>> {
        // will be set to `Some` when we process the first block that the batch can be sealed after
        let mut deadline: Option<Pin<Box<Sleep>>> = None;

        let batch_number = self.prev_batch_info.batch_number + 1;
        let mut blocks: Vec<(BlockOutput, ReplayRecord, TreeBatchOutput, ProverInput)> = vec![];
        let mut accumulator = BatchInfoAccumulator::new(
            self.batcher_config.blocks_per_batch_limit,
            self.pubdata_limit_bytes,
        );

        let mut expected_block_number = expected_first_block;
        loop {
            latency_tracker.enter_state(GenericComponentState::WaitingRecv);
            tokio::select! {
                /* ---------- check for timeout ---------- */
                _ = async {
                    if let Some(d) = &mut deadline {
                        d.as_mut().await
                    }
                }, if deadline.is_some() => {
                    BATCHER_METRICS.seal_reason[&"timeout"].inc();
                    tracing::debug!(batch_number, "Timeout reached, sealing the batch.");
                    break;
                }

                /* ---------- collect blocks ---------- */
               should_seal = block_receiver.peek_recv(|(block_output, replay_record, _, _)| {
                    // determine if the block fits into the current batch
                    accumulator.clone().add(block_output, replay_record).should_seal()
                }) => {
                    latency_tracker.enter_state(GenericComponentState::Processing);
                    match should_seal {
                        Some(true) => {
                            // some of the limits was reached, start sealing the batch
                            break;
                        }
                        Some(false) => {
                            let Some((block_output, replay_record, prover_input, tree)) = block_receiver.pop_buffer() else {
                                anyhow::bail!("No block received in buffer after peeking")
                            };

                            tracing::debug!(
                                batch_number,
                                block_number = replay_record.block_context.block_number,
                                "Adding block to a pending batch."
                            );
                            let block_number = replay_record.block_context.block_number;

                            // sanity check - ensure that we process blocks in order
                            anyhow::ensure!(block_number == expected_block_number,
                                "Unexpected block number received. Expected {expected_block_number}, got {block_number}"
                            );
                            expected_block_number += 1;

                            let (root_hash, leaf_count) = tree.block_end.root_info()?;

                            let tree_output = TreeBatchOutput {
                                root_hash,
                                leaf_count,
                            };

                            // ---------- accumulate batch data ----------
                            accumulator.add(&block_output, &replay_record);

                            blocks.push((
                                block_output,
                                replay_record,
                                tree_output,
                                prover_input,
                            ));

                            // arm the timer after we process the block number that's more or equal
                            // than last persisted one - we don't want to seal on timeout if we know that there are still pending blocks in the inbound channel
                            if deadline.is_none() {
                                if block_number >= self.last_persisted_block {
                                    deadline = Some(Box::pin(tokio::time::sleep(self.batcher_config.batch_timeout)));
                                } else {
                                    tracing::debug!(
                                        block_number,
                                        last_persisted_block = self.last_persisted_block,
                                        "received block with number lower than `last_persisted_block`. Not enabling the deadline seal criteria yet."
                                    )
                                }
                            }
                        }
                        None => {
                            anyhow::bail!("Batcher's block receiver channel closed unexpectedly");
                        }
                    }
                }
            }
        }
        BATCHER_METRICS
            .blocks_per_batch
            .observe(blocks.len() as u64);
        accumulator.report_accumulated_resources_to_metrics();
        /* ---------- seal the batch ---------- */
        let batch_envelope = batch_builder::seal_batch(
            &blocks,
            self.prev_batch_info.clone(),
            batch_number,
            self.chain_id,
            self.chain_address,
        )?;
        self.prev_batch_info = batch_envelope.batch.batch_info.clone().into_stored();
        Ok(batch_envelope)
    }
}
