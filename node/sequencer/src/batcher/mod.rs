use crate::metrics::GENERAL_METRICS;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::watch;
use tracing;
use zk_os_forward_system::run::BlockOutput;
use zksync_os_l1_sender::commitment::{CommitBatchInfo, StoredBatchInfo};
use zksync_os_l1_sender::model::{BatchEnvelope, BatchMetadata, ProverInput, Trace};
use zksync_os_merkle_tree::{MerkleTreeForReading, RocksDBWrapper};
use crate::config::BatcherConfig;
use zksync_os_storage_api::ReplayRecord;

pub mod util;

/// This component handles batching logic - receives blocks and prepares batch data
pub struct Batcher {
    // == initial state ==
    // L2 chain id
    chain_id: u64,
    // first block to process
    first_block_to_process: u64,
    // last persisted block number
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
    pub async fn run_loop(mut self, prev_batch_info: StoredBatchInfo, mut stop_receiver: watch::Receiver<bool>) -> anyhow::Result<()> {
        let mut prev_batch_info = prev_batch_info;

        loop {
            tokio::select! {
                _ = stop_receiver.changed() => {
                    tracing::info!("Batcher received stop signal, shutting down.");
                    break;
                }

                batch = self.create_batch(&prev_batch_info) => {
                    match batch {
                        Ok(batch_envelope) => {
                            prev_batch_info = batch_envelope.batch.commit_batch_info.clone().into();

                            tracing::info!("Batch created successfully, sending to prover input generator.");
                            self.batch_data_sender
                                .send(batch_envelope)
                                .await
                                .map_err(|e| anyhow::anyhow!("Failed to send batch data: {}", e))?;
                        }
                        Err(e) => {
                            tracing::error!("Failed to create batch: {}", e);
                        }
                    }
                }
            }
        }

        Ok(())
    }

    async fn create_batch(&mut self, prev_batch_info: &StoredBatchInfo) -> anyhow::Result<BatchEnvelope<ProverInput>> {
        let mut timer = tokio::time::interval(self.batcher_config.polling_interval);

        let batch_number = prev_batch_info.batch_number + 1;
        let mut blocks: Vec<(BlockOutput, ReplayRecord, zksync_os_merkle_tree::TreeBatchOutput, ProverInput)> = vec![];

        loop {
            tokio::select! {
                /* ---------- check for timeout ---------- */
                _ = timer.tick() => {
                    // no blocks, skip this iteration
                    if blocks.is_empty() {
                        continue;
                    }

                    // if we haven't batched all the blocks that were stored before the restart - no sealing on timeout
                    let (_, last_block, _, _) = blocks.last().unwrap();
                    if last_block.block_context.block_number < self.last_persisted_block {
                        continue;
                    }

                    let (_, first_block, _, _) = blocks.first().unwrap();
                    if self.should_seal_by_timeout(first_block.block_context.timestamp, last_block.block_context.timestamp).await {
                        tracing::info!(batch_number, "Timeout reached, sealing batch.");
                        break;
                    }
                }

                /* ---------- receive blocks ---------- */
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
        let block_number_from = blocks.first().unwrap().1.block_context.block_number;
        let block_number_to = blocks.last().unwrap().1.block_context.block_number;

        let commit_batch_info = CommitBatchInfo::new(
            blocks.iter().map(|(block_output, replay_record, tree, _)|
                (
                    block_output,
                    &replay_record.block_context,
                    replay_record.transactions.as_slice(),
                    tree
                )).collect(),
            self.chain_id,
            batch_number,
        );

        let batch_envelope: BatchEnvelope<ProverInput> = BatchEnvelope {
            batch: BatchMetadata {
                previous_stored_batch_info: prev_batch_info.clone(),
                commit_batch_info,
                first_block_number: block_number_from,
                last_block_number: block_number_to,
                tx_count: blocks.iter().map(|(block_output, _, _, _)| block_output.tx_results.len()).sum(),
            },
            data: std::iter::once(u32::try_from(blocks.len()).expect("too many blocks"))
                .chain(blocks.iter().flat_map(|(_, _, _, prover_input)| prover_input.iter().copied()))
                .collect(),
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

    async fn should_seal_by_timeout(&self, first_block_timestamp: u64, last_block_timestamp: u64) -> bool {
        let time_between_first_and_last = SystemTime::from(UNIX_EPOCH + Duration::from_secs(last_block_timestamp))
            .duration_since(SystemTime::from(UNIX_EPOCH + Duration::from_secs(first_block_timestamp)))
            .expect("Current time is before the start timestamp");

        time_between_first_and_last >= self.batcher_config.batch_timeout
    }

    /// Checks if the batch should be sealed based on the content of the blocks.
    /// e.g. due to the block count limit, tx count limit, or pubdata size limit.
    async fn should_seal_by_content(&self) -> bool {
        false // TODO: add sealing criteria
    }
}
