use crate::batcher::seal_criteria::BatchInfoAccumulator;
use crate::config::BatcherConfig;
use crate::prover_api::proof_storage::ProofStorage;
use alloy::primitives::Address;
use anyhow::Context;
use async_trait::async_trait;
use std::pin::Pin;
use tokio::sync::mpsc;
use tokio::time::Sleep;
use tracing;
use zksync_os_batch_types::BlockMerkleTreeData;
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

/// Set of fields to define batcher's behavior on startup (when to replay, when to produce, etc.)
pub struct BatcherStartupConfig {
    /// Last committed block on L1. Blocks below this are part of already-committed batches.
    /// Blocks before this one (inclusive)
    /// need to be recreated and matched with what we have in persistence.
    /// Blocks after this will be created anew.
    /// Note that there may have been batches created for even never blocks,
    /// but we don't consider them final before they are committed and lose them on block restart.
    pub last_committed_block: u64,
    /// Info for the previous batch - the batch that was emitted just before the first block we'll process.
    /// Required to set correct StoredBatchInfo when committing/proving/executing blocks on L1.
    pub prev_batch_info: StoredBatchInfo,
    /// Last block number already known to this node. On startup, we'll replay all blocks until and including
    /// this - in other words, there will be no arbitrary delays until this block is passed through Batcher.
    /// We do not seal batches by timeout until this block is reached.
    /// This helps to avoid premature sealing due to timeout criterion, since for every tick of the
    /// timer the `should_seal_by_timeout` will often return `true`
    /// (because those blocks were produced during the previous run of the node - maybe some time ago)
    pub last_persisted_block: u64,
}

/// Batcher component - handles batching logic, receives blocks and prepares batch data
pub struct Batcher {
    pub startup_config: BatcherStartupConfig,
    pub chain_id: u64,
    pub chain_address: Address,
    pub pubdata_limit_bytes: u64,
    pub batcher_config: BatcherConfig,
    pub batch_storage: ProofStorage,
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

        let mut prev_batch_info = self.startup_config.prev_batch_info.clone();

        loop {
            latency_tracker.enter_state(GenericComponentState::WaitingRecv);

            // Peek at the next block to decide whether to recreate or create anew.
            let next_block_number = input
                .peek_recv(|(_, replay_record, _, _)| replay_record.block_context.block_number)
                .await
                .context("batcher inbound channel unexpectedly closed")?;
            latency_tracker.enter_state(GenericComponentState::Processing);

            let should_recreate = next_block_number <= self.startup_config.last_committed_block;

            let batch_envelope = if should_recreate {
                self.recreate_existing_batch(&mut input, &latency_tracker, &prev_batch_info)
                    .await?
            } else {
                self.create_batch(&mut input, &latency_tracker, &prev_batch_info)
                    .await?
            };

            // Update prev_batch_info for the next iteration
            prev_batch_info = batch_envelope.batch.batch_info.clone().into_stored();

            BATCHER_METRICS
                .transactions_per_batch
                .observe(batch_envelope.batch.tx_count as u64);

            tracing::info!(
                batch_number = batch_envelope.batch_number(),
                batch_metadata = ?batch_envelope.batch,
                block_count = batch_envelope.batch.last_block_number - batch_envelope.batch.first_block_number + 1,
                new_state_commitment = ?batch_envelope.batch.batch_info.new_state_commitment,
                "Batch {}", if should_recreate { "recreated" } else { "created" }
            );

            tracing::debug!(
                batch_number = batch_envelope.batch_number(),
                da_commitment = ?batch_envelope.batch.batch_info.operator_da_input,
                "Batch da_input",
            );

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
        prev_batch_info: &StoredBatchInfo,
    ) -> anyhow::Result<BatchForSigning<ProverInput>> {
        // will be set to `Some` when we process the first block that the batch can be sealed after
        let mut deadline: Option<Pin<Box<Sleep>>> = None;

        let batch_number = prev_batch_info.batch_number + 1;
        let mut blocks: Vec<(BlockOutput, ReplayRecord, TreeBatchOutput, ProverInput)> = vec![];
        let mut accumulator = BatchInfoAccumulator::new(
            self.batcher_config.blocks_per_batch_limit,
            self.pubdata_limit_bytes,
        );

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

                            let block_number = replay_record.block_context.block_number;

                            tracing::debug!(
                                batch_number,
                                block_number,
                                "Adding block to a pending batch."
                            );

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
                                if block_number >= self.startup_config.last_persisted_block {
                                    deadline = Some(Box::pin(tokio::time::sleep(self.batcher_config.batch_timeout)));
                                } else {
                                    tracing::debug!(
                                        block_number,
                                        last_persisted_block = self.startup_config.last_persisted_block,
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
            prev_batch_info.clone(),
            batch_number,
            self.chain_id,
            self.chain_address,
        )?;
        Ok(batch_envelope)
    }

    async fn recreate_existing_batch(
        &mut self,
        block_receiver: &mut PeekableReceiver<(
            BlockOutput,
            ReplayRecord,
            ProverInput,
            BlockMerkleTreeData,
        )>,
        latency_tracker: &ComponentStateHandle<GenericComponentState>,
        prev_batch_info: &StoredBatchInfo,
    ) -> anyhow::Result<BatchForSigning<ProverInput>> {
        let batch_number = prev_batch_info.batch_number + 1;

        // Load the existing batch from storage - we'll rebuild it and verify it matches
        let existing_batch = self
            .batch_storage
            .get_batch_with_proof(batch_number)
            .await?
            .context(format!(
                "Batch {batch_number} that is being rebuilt should exist in storage"
            ))?;

        tracing::info!(
            batch_number,
            first_block = existing_batch.batch.first_block_number,
            last_block = existing_batch.batch.last_block_number,
            "Recreating existing batch"
        );

        let mut blocks: Vec<(BlockOutput, ReplayRecord, TreeBatchOutput, ProverInput)> = vec![];

        let expected_block_count =
            existing_batch.batch.last_block_number - existing_batch.batch.first_block_number + 1;
        // Collect all blocks in this batch
        while blocks.len() < expected_block_count as usize {
            latency_tracker.enter_state(GenericComponentState::WaitingRecv);
            let (block_output, replay_record, prover_input, tree) = block_receiver
                .recv()
                .await
                .context("channel closed while recreating batch")?;
            latency_tracker.enter_state(GenericComponentState::Processing);

            let (root_hash, leaf_count) = tree.block_end.root_info()?;
            let tree_output = TreeBatchOutput {
                root_hash,
                leaf_count,
            };

            tracing::debug!(
                batch_number,
                block_number = replay_record.block_context.block_number,
                "Adding block to recreated batch"
            );

            blocks.push((block_output, replay_record, tree_output, prover_input));
        }
        let last_block_number = blocks.last().unwrap().0.header.number;
        assert_eq!(
            last_block_number, existing_batch.batch.last_block_number,
            "Block number mismatch in last block of a rebuilt batch"
        );

        // Rebuild the batch from blocks
        let rebuilt_batch = batch_builder::seal_batch(
            &blocks,
            prev_batch_info.clone(),
            batch_number,
            self.chain_id,
            self.chain_address,
        )?;

        // Verify that the rebuilt batch matches the stored batch by comparing hashes
        let rebuilt_stored_batch_info = rebuilt_batch.batch.batch_info.clone().into_stored();
        let stored_stored_batch_info = existing_batch.batch.batch_info.clone().into_stored();

        anyhow::ensure!(
            rebuilt_stored_batch_info.hash() == stored_stored_batch_info.hash(),
            "Rebuilt batch info does not match stored batch info for batch {}. \
             Rebuilt info: {:?}, Stored info: {:?}",
            batch_number,
            rebuilt_stored_batch_info,
            stored_stored_batch_info
        );

        Ok(rebuilt_batch)
    }
}
