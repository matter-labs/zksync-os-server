mod util;

use crate::CHAIN_ID;
use crate::metrics::GENERAL_METRICS;
use crate::model::{BatchJob, ReplayRecord};
use alloy::primitives::B256;
use futures::{FutureExt, StreamExt, TryStreamExt};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc::Receiver;
use tokio_stream::wrappers::ReceiverStream;
use vise::{Buckets, Gauge, Histogram, LabeledFamily, Metrics, Unit};
use zk_os_forward_system::run::test_impl::TxListSource;
use zk_os_forward_system::run::{BatchOutput, StorageCommitment, generate_proof_input};
use zksync_os_l1_sender::L1SenderHandle;
use zksync_os_l1_sender::commitment::{CommitBatchInfo, StoredBatchInfo};
use zksync_os_merkle_tree::{
    MerkleTreeForReading, MerkleTreeVersion, RocksDBWrapper, fixed_bytes_to_bytes32,
};
use zksync_os_state::StateHandle;
use zksync_os_types::ZksyncOsEncode;

pub mod util;

/// This component generates l1 batches from the stream of blocks
/// It also generates Prover Input for each batch.
///
/// Currently, batching is not implemented on zksync-os side, so we do 1 batch == 1 block
/// Thus, this component only generates prover input.
pub struct Batcher {
    block_receiver: Receiver<(BatchOutput, ReplayRecord)>,
    // todo: the following two may just need to be a broadcast with backpressure instead (to eth-sender and prover-api)
    batch_sender: tokio::sync::mpsc::Sender<BatchJob>,
    // handled by l1-sender. We ensure that they are sent in order.
    commit_batch_info_sender: Option<L1SenderHandle>,
    persistent_tree: MerkleTreeForReading<RocksDBWrapper>,
    state_handle: StateHandle,
    last_committed_batch: u64,
    bin_path: &'static str,
    maximum_in_flight_blocks: usize,
    // number and state commitment of the last processed batch.
    prev_batch_data: (u64, B256),
}

impl Batcher {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        block_receiver: tokio::sync::mpsc::Receiver<(BatchOutput, ReplayRecord)>,
        batch_sender: tokio::sync::mpsc::Sender<BatchJob>,
        // handled by l1-sender
        commit_batch_info_sender: Option<L1SenderHandle>,
        state_handle: StateHandle,
        persistent_tree: MerkleTreeForReading<RocksDBWrapper>,
        last_committed_batch: u64,

        enable_logging: bool,
        maximum_in_flight_blocks: usize,
        prev_batch_data: (u64, B256),
    ) -> Self {
        // Use path relative to crate's Cargo.toml to ensure consistent pathing in different contexts
        let bin_path = if enable_logging {
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../server_app_logging_enabled.bin"
            )
        } else {
            concat!(env!("CARGO_MANIFEST_DIR"), "/../../server_app.bin")
        };
        Self {
            block_receiver,
            state_handle,
            batch_sender,
            persistent_tree,
            commit_batch_info_sender,
            last_committed_batch,
            bin_path,
            maximum_in_flight_blocks,
            prev_batch_data,
        }
    }

    /// Works on multiple blocks in parallel. May use up to [Self::maximum_in_flight_blocks] threads but
    /// will only take up new work once the oldest block finishes processing.
    pub async fn run_loop(self) -> anyhow::Result<()> {
        ReceiverStream::new(self.block_receiver)
            .then(|(batch_output, replay_record)| {
                self.persistent_tree
                    .clone()
                    .get_at_block(replay_record.block_context.block_number - 1)
                    .map(|tree| (tree, batch_output, replay_record))
            })
            .map(|(tree, batch_output, replay_record)| {
                BATCHER_METRICS
                    .current_block_number
                    .set(replay_record.block_context.block_number);
                let block_number = replay_record.block_context.block_number;
                tracing::debug!(
                    "Batcher started processing block {} with {} transactions",
                    block_number,
                    replay_record.transactions.len(),
                );

                let state_handle = self.state_handle.clone();
                tokio::task::spawn_blocking(move || {
                    (
                        compute_prover_input(&replay_record, state_handle, tree, self.bin_path),
                        batch_output,
                        replay_record,
                    )
                })
            })
            .buffered(self.maximum_in_flight_blocks)
            .map_err(|e| anyhow::anyhow!(e))
            .try_fold(
                util::load_genesis_stored_batch(),
                async |prev_batch, (prover_input, batch_output, replay_record)| {
                    let block_number = replay_record.block_context.block_number;
                    assert_eq!(prev_batch.batch_number + 1, block_number);

                    let (root_hash, leaf_count) = self
                        .persistent_tree
                        .clone()
                        .get_at_block(block_number)
                        .await
                        .root_info()
                        .unwrap();

                    let tree_output = zksync_os_merkle_tree::BatchOutput {
                        root_hash,
                        leaf_count,
                    };

                    let tx_count = replay_record.transactions.len();
                    let commit_batch_info = CommitBatchInfo::new(
                        batch_output,
                        &replay_record.block_context,
                        &replay_record.transactions,
                        tree_output,
                        CHAIN_ID,
                    );
                    tracing::debug!("Expected commit batch info: {:?}", commit_batch_info);

                    let stored_batch_info = StoredBatchInfo::from(commit_batch_info.clone());
                    tracing::debug!("Expected stored batch info: {:?}", stored_batch_info);

                    GENERAL_METRICS.block_number[&"batcher"].set(block_number);
                    GENERAL_METRICS.executed_transactions[&"batcher"].inc_by(tx_count as u64);

                    let previous_state_commitment = stored_batch_info.state_commitment;
                    if commit_batch_info.batch_number > self.last_committed_batch {
                        if let Some(l1) = &self.commit_batch_info_sender {
                            l1.commit(prev_batch, commit_batch_info.clone()).await?;
                        }
                    }
                    self.batch_sender
                        .send(BatchJob {
                            block_number,
                            prover_input,
                            previous_state_commitment,
                            commit_batch_info,
                        })
                        .await?;

                    Ok(stored_batch_info)
                },
            )
            .await
            .map(|_| ())
    }
}

fn compute_prover_input(
    replay_record: &ReplayRecord,
    state_handle: StateHandle,
    tree_view: MerkleTreeVersion<RocksDBWrapper>,
    bin_path: &'static str,
) -> Vec<u32> {
    let block_number = replay_record.block_context.block_number;

    let (root_hash, leaf_count) = tree_view.root_info().unwrap();
    let initial_storage_commitment = StorageCommitment {
        root: fixed_bytes_to_bytes32(root_hash),
        next_free_slot: leaf_count,
    };

    let state_view = state_handle.state_view_at_block(block_number - 1).unwrap();

    let transactions = replay_record
        .transactions
        .iter()
        .map(|tx| tx.clone().encode())
        .collect::<VecDeque<_>>();
    let list_source = TxListSource { transactions };

    let prover_input_generation_latency =
        BATCHER_METRICS.prover_input_generation[&"prover_input_generation"].start();
    let prover_input = generate_proof_input(
        PathBuf::from(bin_path),
        replay_record.block_context,
        initial_storage_commitment,
        tree_view,
        state_view,
        list_source,
    )
    .expect("proof gen failed");

    let latency = prover_input_generation_latency.observe();

    tracing::info!(
        block_number,
        next_free_slot = leaf_count,
        "Completed prover input computation in {:?}.",
        latency
    );

    prover_input
}

const LATENCIES_FAST: Buckets = Buckets::exponential(0.0000001..=1.0, 2.0);
#[derive(Debug, Metrics)]
#[metrics(prefix = "batcher")]
pub struct BatcherMetrics {
    #[metrics(unit = Unit::Seconds, labels = ["stage"], buckets = LATENCIES_FAST)]
    pub prover_input_generation: LabeledFamily<&'static str, Histogram<Duration>>,

    pub current_block_number: Gauge<u64>,
}

#[vise::register]
pub(crate) static BATCHER_METRICS: vise::Global<BatcherMetrics> = vise::Global::new();
