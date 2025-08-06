use crate::config::BatcherConfig;
use crate::metrics::GENERAL_METRICS;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc::{Receiver, Sender};
use tracing;
use zk_os_forward_system::run::BlockOutput;
use zksync_os_l1_sender::commitment::{CommitBatchInfo, StoredBatchInfo};
use zksync_os_l1_sender::model::{BatchEnvelope, BatchMetadata, ProverInput, Trace};
use zksync_os_merkle_tree::{MerkleTreeForReading, RocksDBWrapper};
use zksync_os_storage_api::ReplayRecord;

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

            tracing::info!("Batch created successfully, sending to prover input generator.");
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
        let mut timer = tokio::time::interval(self.batcher_config.polling_interval);

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
                _ = timer.tick() => {
                   let (
                        Some((_, first_block, _, _)),
                        Some((_, last_block, _, _))
                    ) = (blocks.first(), blocks.last()) else {
                        // no blocks, skip this iteration
                        continue;
                    };

                    // if we haven't batched all the blocks that were stored before the restart - no sealing by timeout
                    if last_block.block_context.block_number < self.last_persisted_block {
                        continue;
                    }

                    if self.should_seal_by_timeout(first_block.block_context.timestamp).await {
                        tracing::debug!(batch_number, "Timeout reached, sealing batch.");
                        break;
                    }
                }

                /* ---------- collect blocks ---------- */
                maybe_block = self.block_receiver.recv() => {
                    match maybe_block {
                        Some((block_output, replay_record, prover_input)) => {
                            let block_number = replay_record.block_context.block_number;

                            // skip the blocks from committed batches
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
        seal_batch(
            &blocks,
            prev_batch_info.clone(),
            batch_number,
            self.chain_id,
        )
    }

    async fn should_seal_by_timeout(&self, first_block_timestamp: u64) -> bool {
        let time_between_first_and_last = SystemTime::now()
            .duration_since(UNIX_EPOCH + Duration::from_secs(first_block_timestamp))
            .expect("Current time is before the start timestamp");

        time_between_first_and_last >= self.batcher_config.batch_timeout
    }

    /// Checks if the batch should be sealed based on the content of the blocks.
    /// e.g. due to the block count limit, tx count limit, or pubdata size limit.
    async fn should_seal_by_content(&self) -> bool {
        false // TODO: add sealing criteria
    }
}

/// Takes a vector of blocks and produces a batch envelope.
fn seal_batch(
    blocks: &[(
        BlockOutput,
        ReplayRecord,
        zksync_os_merkle_tree::TreeBatchOutput,
        ProverInput,
    )],
    prev_batch_info: StoredBatchInfo,
    batch_number: u64,
    chain_id: u64,
) -> anyhow::Result<BatchEnvelope<ProverInput>> {
    let block_number_from = blocks.first().unwrap().1.block_context.block_number;
    let block_number_to = blocks.last().unwrap().1.block_context.block_number;

    let commit_batch_info = CommitBatchInfo::new(
        blocks
            .iter()
            .map(|(block_output, replay_record, tree, _)| {
                (
                    block_output,
                    &replay_record.block_context,
                    replay_record.transactions.as_slice(),
                    tree,
                )
            })
            .collect(),
        chain_id,
        batch_number,
    );

    // batch prover input is a concatenation of all blocks' prover inputs with the prepended block count
    let batch_prover_input: ProverInput =
        std::iter::once(u32::try_from(blocks.len()).expect("too many blocks"))
            .chain(
                blocks
                    .iter()
                    .flat_map(|(_, _, _, prover_input)| prover_input.iter().copied()),
            )
            .collect();

    let batch_envelope: BatchEnvelope<ProverInput> = BatchEnvelope {
        batch: BatchMetadata {
            previous_stored_batch_info: prev_batch_info,
            commit_batch_info,
            first_block_number: block_number_from,
            last_block_number: block_number_to,
            tx_count: blocks
                .iter()
                .map(|(block_output, _, _, _)| block_output.tx_results.len())
                .sum(),
        },
        data: batch_prover_input,
        trace: Trace::default(),
    };

    tracing::info!(
        block_number_from,
        block_number_to,
        batch_number,
        state_commitment = ?batch_envelope.batch.commit_batch_info.new_state_commitment,
        "Batch produced",
    );

    tracing::debug!(
        ?batch_envelope.batch,
        "Batch details",
    );

    Ok(batch_envelope)
}
