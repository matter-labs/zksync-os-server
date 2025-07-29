use crate::metrics::GENERAL_METRICS;
use crate::model::batches::{BatchEnvelope, BatchMetadata, Trace};
use crate::model::blocks::ReplayRecord;
use crate::prover_input_generator::ProverInputGeneratorBatchData;
use futures::{FutureExt, StreamExt, TryStreamExt};
use std::future::ready;
use std::path::PathBuf;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio_stream::wrappers::ReceiverStream;
use tracing;
use zk_os_forward_system::run::BlockOutput;
use zksync_os_l1_sender::commitment::{CommitBatchInfo, StoredBatchInfo};
use zksync_os_merkle_tree::{MerkleTreeForReading, RocksDBWrapper};

mod batcher_rocks_db_storage;
pub mod util;

/// This component handles batching logic - receives blocks and prepares batch data
/// todo: we still do 1 batch == 1 block, implement proper batching
pub struct Batcher {
    // == state ==
    // holds info about the last processed batch (genesis if none)
    prev_batch_info: StoredBatchInfo,
    // only used on startup. Skips all upstream blocks until this one.
    batcher_starting_block: u64,
    // L2 chain id
    chain_id: u64,

    // == persistence (todo: get rid of it - see zksync-os-server/README.md for details) - only used to recover initial `prev_batch_info` ==
    storage: batcher_rocks_db_storage::BatcherRocksDBStorage,

    // == plumbing ==
    // inbound
    block_receiver: Receiver<(BlockOutput, ReplayRecord)>,
    // outbound
    batch_data_sender: Sender<BatchEnvelope<ProverInputGeneratorBatchData>>,
    // dependencies
    persistent_tree: MerkleTreeForReading<RocksDBWrapper>,
}

impl Batcher {
    pub fn new(
        // == initial state ==
        batcher_starting_block: u64,
        chain_id: u64,

        // == config ==
        rocks_db_path: PathBuf,

        // == plumbing ==
        block_receiver: Receiver<(BlockOutput, ReplayRecord)>,
        batch_data_sender: Sender<BatchEnvelope<ProverInputGeneratorBatchData>>,
        persistent_tree: MerkleTreeForReading<RocksDBWrapper>,
    ) -> Self {
        // todo: will not need storage in the future
        let storage = batcher_rocks_db_storage::BatcherRocksDBStorage::new(rocks_db_path);

        let prev_batch_info = if batcher_starting_block == 1 {
            util::genesis_stored_batch_info()
        } else {
            storage
                .get(batcher_starting_block - 1)
                .expect("cannot access batcher storage")
                .expect("no prev batch info")
        };

        Self {
            block_receiver,
            batch_data_sender,
            persistent_tree,
            batcher_starting_block,
            storage,
            prev_batch_info,
            chain_id,
        }
    }

    /// Main processing loop for the batcher
    pub async fn run_loop(self) -> anyhow::Result<()> {
        ReceiverStream::new(self.block_receiver)
            .skip_while(move |(_, record)| {
                let skip = record.block_context.block_number < self.batcher_starting_block;
                if skip {
                    tracing::debug!(
                        "Skipping block {} (batcher starting block is {})",
                        record.block_context.block_number,
                        self.batcher_starting_block
                    );
                }
                ready(skip)
            })
            // wait for tree to have processed block
            .then(|(block_output, replay_record)| {
                self.persistent_tree
                    .clone()
                    .get_at_block(replay_record.block_context.block_number)
                    .map(|tree| Ok::<_, anyhow::Error>((tree, block_output, replay_record)))
            })
            .try_fold(
                self.prev_batch_info,
                async |prev_batch_info, (tree, block_output, replay_record)| {
                    let block_number = replay_record.block_context.block_number;

                    let (root_hash, leaf_count) = tree.root_info().unwrap();

                    let tree_output = zksync_os_merkle_tree::TreeBatchOutput {
                        root_hash,
                        leaf_count,
                    };

                    let tx_count = replay_record.transactions.len();

                    let commit_batch_info = CommitBatchInfo::new(
                        block_output,
                        &replay_record.block_context,
                        &replay_record.transactions,
                        tree_output,
                        self.chain_id,
                    );

                    let stored_batch_info = StoredBatchInfo::from(commit_batch_info.clone());

                    // Store batch info (todo: will not need storage in the future)
                    self.storage
                        .set(commit_batch_info.batch_number, &stored_batch_info)?;

                    // Create batch
                    let batch_envelope = BatchEnvelope {
                        batch: BatchMetadata {
                            previous_stored_batch_info: prev_batch_info.clone(),
                            commit_batch_info,
                            first_block_number: block_number,
                            last_block_number: block_number,
                            tx_count,
                        },
                        data: ProverInputGeneratorBatchData {
                            previous_block_timestamp: prev_batch_info.last_block_timestamp,
                            replay_records: vec![replay_record]
                        },
                        trace: Trace::default(),
                    };

                    tracing::info!(
                        block_number_from = block_number,
                        block_number_to = block_number,
                        batch_number = block_number,
                        state_commitment = ?batch_envelope.batch.commit_batch_info.new_state_commitment,
                        "Batch produced",
                    );

                    tracing::debug!(
                        ?batch_envelope.batch,
                        "Batch details",
                    );

                    GENERAL_METRICS.block_number[&"batcher"].set(block_number);

                    // Send to ProverInputGenerator
                    self.batch_data_sender
                        .send(batch_envelope)
                        .await
                        .map_err(|e| anyhow::anyhow!("Failed to send batch data: {}", e))?;
                    Ok(stored_batch_info)
                },
            )
            .await
            .map(|_| ())
    }
}
