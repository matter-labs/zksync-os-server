use crate::batcher::seal_criteria::BatchInfoAccumulator;
use crate::config::BatcherConfig;
use crate::util::peekable_receiver::PeekableReceiver;
use alloy::primitives::Address;
use std::pin::Pin;
use tokio::sync::mpsc::Sender;
use tokio::time::Sleep;
use tracing;
use zk_os_forward_system::run::BlockOutput;
use zksync_os_l1_sender::batcher_metrics::BATCHER_METRICS;
use zksync_os_l1_sender::batcher_model::{BatchEnvelope, ProverInput};
use zksync_os_l1_sender::commitment::{PubdataSource, StoredBatchInfo};
use zksync_os_merkle_tree::{MerkleTreeForReading, RocksDBWrapper, TreeBatchOutput};
use zksync_os_observability::{
    ComponentStateHandle, ComponentStateReporter, GenericComponentState,
};
use zksync_os_storage_api::ReplayRecord;

mod batch_builder;
mod seal_criteria;
pub mod util;

/// This component handles batching logic - receives blocks and prepares batch data.
pub struct Batcher {
    // == initial state ==
    // L2 chain id
    chain_id: u64,
    chain_address: Address,
    // first block to process
    first_block_to_process: u64,
    /// Last persisted block. We should not seal batches by timeout until this block is reached.
    /// This helps to avoid premature sealing due to timeout criterion, since for  every tick of the
    /// timer the `should_seal_by_timeout` call will return `true`
    last_persisted_block: u64,

    // == config ==
    pubdata_limit_bytes: u64,
    batcher_config: BatcherConfig,

    // == plumbing ==
    // inbound
    block_receiver: PeekableReceiver<(BlockOutput, ReplayRecord, ProverInput)>,
    // outbound
    batch_data_sender: Sender<BatchEnvelope<ProverInput>>,
    // dependencies
    persistent_tree: MerkleTreeForReading<RocksDBWrapper>,
}

impl Batcher {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        // == initial state ==
        chain_id: u64,
        chain_address: Address,
        first_block_to_process: u64,
        last_persisted_block: u64,

        // == config ==
        pubdata_limit_bytes: u64,
        batcher_config: BatcherConfig,

        // == plumbing ==
        block_receiver: PeekableReceiver<(BlockOutput, ReplayRecord, ProverInput)>,
        batch_data_sender: Sender<BatchEnvelope<ProverInput>>,
        persistent_tree: MerkleTreeForReading<RocksDBWrapper>,
    ) -> Self {
        Self {
            chain_id,
            chain_address,
            first_block_to_process,
            last_persisted_block,
            pubdata_limit_bytes,
            batcher_config,
            block_receiver,
            batch_data_sender,
            persistent_tree,
        }
    }

    /// Main processing loop for the batcher
    pub async fn run_loop(mut self, mut prev_batch_info: StoredBatchInfo) -> anyhow::Result<()> {
        let latency_tracker = ComponentStateReporter::global()
            .handle_for("batcher", GenericComponentState::WaitingRecv);

        let mut first_block_in_batch = self.first_block_to_process;
        loop {
            let batch_envelope = self
                .create_batch(&prev_batch_info, &latency_tracker, first_block_in_batch)
                .await?;
            BATCHER_METRICS
                .transactions_per_batch
                .observe(batch_envelope.batch.tx_count as u64);
            prev_batch_info = batch_envelope.batch.commit_batch_info.clone().into();

            tracing::info!(
                number = batch_envelope.batch_number(),
                block_from = batch_envelope.batch.first_block_number,
                block_to = batch_envelope.batch.last_block_number,
                tx_count = batch_envelope.batch.tx_count,
                new_state_commitment = ?batch_envelope.batch.commit_batch_info.new_state_commitment,
                "Batch created"
            );

            first_block_in_batch = batch_envelope.batch.last_block_number + 1;
            latency_tracker.enter_state(GenericComponentState::WaitingSend);
            self.batch_data_sender
                .send(batch_envelope)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to send batch data: {}", e))?;
        }
    }

    async fn create_batch(
        &mut self,
        prev_batch_info: &StoredBatchInfo,
        latency_tracker: &ComponentStateHandle<GenericComponentState>,
        expected_first_block: u64,
    ) -> anyhow::Result<BatchEnvelope<ProverInput>> {
        // will be set to `Some` when we process the first block that the batch can be sealed after
        let mut deadline: Option<Pin<Box<Sleep>>> = None;

        let batch_number = prev_batch_info.batch_number + 1;
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
                should_seal = self.block_receiver.peek_recv(|(block_output, _, _)| {
                    // determine if the block fits into the current batch
                    accumulator.clone().add(block_output).is_batch_limit_reached()
                }) => {
                    latency_tracker.enter_state(GenericComponentState::Processing);
                    match should_seal {
                        Some(true) => {
                            // some of the limits was reached, start sealing the batch
                            break;
                        }
                        Some(false) => {
                            let Some((block_output, replay_record, prover_input)) = self.block_receiver.pop_buffer() else {
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
                                "Unexpected block number received. Expected {}, got {}",
                                expected_block_number,
                                block_number,
                            );
                            expected_block_number += 1;

                            /* ---------- process block ---------- */
                            let tree = self.persistent_tree
                                .clone()
                                .get_at_block(block_number)
                                .await;

                            let (root_hash, leaf_count) = tree.root_info()?;

                            let tree_output = TreeBatchOutput {
                                root_hash,
                                leaf_count,
                            };

                            // ---------- accumulate batch data ----------
                            accumulator.add(&block_output);

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
                                        self.last_persisted_block,
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
        batch_builder::seal_batch(
            &blocks,
            prev_batch_info.clone(),
            batch_number,
            self.chain_id,
            self.chain_address,
            PubdataSource::try_from(self.batcher_config.pubdata_source).unwrap(),
        )
    }
}
