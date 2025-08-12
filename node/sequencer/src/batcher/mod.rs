use crate::config::BatcherConfig;
use crate::metrics::GENERAL_METRICS;
use std::pin::Pin;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::time::Sleep;
use tracing;
use zk_os_forward_system::run::BlockOutput;
use zksync_os_l1_sender::commitment::StoredBatchInfo;
use zksync_os_l1_sender::model::{BatchEnvelope, ProverInput};
use zksync_os_merkle_tree::{MerkleTreeForReading, RocksDBWrapper};
use zksync_os_storage_api::ReplayRecord;

mod batch_builder;
pub mod util;

/// This component handles batching logic - receives blocks and prepares batch data.
pub struct Batcher {
    // == initial state ==
    // L2 chain id
    chain_id: u64,
    // first block to process
    first_block_to_process: u64,
    /// Last persisted block. We should not seal batches by timeout until this block is reached.
    /// This helps to avoid premature sealing due to timeout criterion, since for  every tick of the
    /// timer the `should_seal_by_timeout` call will return `true`
    last_persisted_block: u64,

    // == config ==
    batcher_config: BatcherConfig,

    // == plumbing ==
    // inbound
    block_receiver: Receiver<(BlockOutput, ReplayRecord, ProverInput)>,
    // outbound
    batch_data_sender: Sender<BatchEnvelope<ProverInput>>,
    // dependencies
    persistent_tree: MerkleTreeForReading<RocksDBWrapper>,
}

impl Batcher {
    pub fn new(
        // == initial state ==
        chain_id: u64,
        first_block_to_process: u64,
        last_persisted_block: u64,

        // == config ==
        batcher_config: BatcherConfig,

        // == plumbing ==
        block_receiver: Receiver<(BlockOutput, ReplayRecord, ProverInput)>,
        batch_data_sender: Sender<BatchEnvelope<ProverInput>>,
        persistent_tree: MerkleTreeForReading<RocksDBWrapper>,
    ) -> Self {
        Self {
            chain_id,
            first_block_to_process,
            last_persisted_block,
            batcher_config,
            block_receiver,
            batch_data_sender,
            persistent_tree,
        }
    }

    /// Main processing loop for the batcher
    pub async fn run_loop(mut self, mut prev_batch_info: StoredBatchInfo) -> anyhow::Result<()> {
        loop {
            let batch_envelope = self.create_batch(&prev_batch_info).await?;
            prev_batch_info = batch_envelope.batch.commit_batch_info.clone().into();

            tracing::info!(
                number = batch_envelope.batch_number(),
                block_from = batch_envelope.batch.first_block_number,
                block_to = batch_envelope.batch.last_block_number,
                tx_count = batch_envelope.batch.tx_count,
                new_state_commitment = batch_envelope.batch.commit_batch_info.new_state_commitment,
                "Batch created"
            );
            self.batch_data_sender
                .send(batch_envelope)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to send batch data: {}", e))?;
        }
    }

    async fn create_batch(
        &mut self,
        prev_batch_info: &StoredBatchInfo,
    ) -> anyhow::Result<BatchEnvelope<ProverInput>> {
        // will be set to `Some` when we process the first block that the batch can be sealed after
        let mut deadline: Option<Pin<Box<Sleep>>> = None;

        let batch_number = prev_batch_info.batch_number + 1;
        let mut blocks: Vec<(
            BlockOutput,
            ReplayRecord,
            zksync_os_merkle_tree::TreeBatchOutput,
            ProverInput,
        )> = vec![];

        loop {
            tokio::select! {
                /* ---------- check for timeout ---------- */
                _ = async {
                    if let Some(d) = &mut deadline {
                        d.as_mut().await
                    }
                }, if deadline.is_some() => {
                    tracing::debug!(batch_number, "Timeout reached, sealing the batch.");
                    break;
                }

                /* ---------- collect blocks ---------- */
                maybe_block = self.block_receiver.recv() => {
                    match maybe_block {
                        Some((block_output, replay_record, prover_input)) => {
                            let block_number = replay_record.block_context.block_number;

                            // skip the blocks from already committed batches (on server restart we replay some historical blocks - they are already batched and committed, so no action is needed here)
                            if block_number < self.first_block_to_process {
                                tracing::debug!(
                                    "Skipping block {} (batcher starting block is {})",
                                    replay_record.block_context.block_number,
                                    self.first_block_to_process
                                );

                                continue;
                            }

                            /* ---------- process block ---------- */
                            let tree = self.persistent_tree
                                .clone()
                                .get_at_block(replay_record.block_context.block_number)
                                .await;

                            let (root_hash, leaf_count) = tree.root_info()?;

                            let tree_output = zksync_os_merkle_tree::TreeBatchOutput {
                                root_hash,
                                leaf_count,
                            };

                            GENERAL_METRICS.block_number[&"batcher"].set(block_number);
                            blocks.push((
                                block_output,
                                replay_record,
                                tree_output,
                                prover_input,
                            ));

                            // arm the timer after we process the block number that's more or equal
                            // than last persisted one
                            if deadline.is_none() && block_number >= self.last_persisted_block {
                                deadline = Some(Box::pin(tokio::time::sleep(self.batcher_config.batch_timeout)));
                            }

                            if self.should_seal_by_content().await {
                                tracing::info!(batch_number, "Content limit reached, sealing batch.");
                                break;
                            }
                        }
                        None => {
                            anyhow::bail!("Batcher's block receiver channel closed unexpectedly");
                        }
                    }
                }
            }
        }

        /* ---------- seal the batch ---------- */
        batch_builder::seal_batch(
            &blocks,
            prev_batch_info.clone(),
            batch_number,
            self.chain_id,
        )
    }

    /// Checks if the batch should be sealed based on the content of the blocks.
    /// e.g. due to the block count limit, tx count limit, or pubdata size limit.
    async fn should_seal_by_content(&self) -> bool {
        false // TODO: add sealing criteria
    }
}
