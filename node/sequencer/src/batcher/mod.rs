use crate::metrics::GENERAL_METRICS;
use crate::model::{BatchJob, ReplayRecord};
use crate::CHAIN_ID;
use futures::{StreamExt, TryStreamExt};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::{mpsc::Receiver, watch};
use tokio_stream::wrappers::ReceiverStream;
use vise::{Buckets, Gauge, Histogram, LabeledFamily, Metrics, Unit};
use zk_os_forward_system::run::test_impl::TxListSource;
use zk_os_forward_system::run::{generate_proof_input, BatchOutput, StorageCommitment};
use zksync_os_l1_sender::commitment::{CommitBatchInfo, StoredBatchInfo};
use zksync_os_l1_sender::L1SenderHandle;
use zksync_os_merkle_tree::{
    fixed_bytes_to_bytes32, MerkleTree, MerkleTreeVersion, RocksDBWrapper,
};
use zksync_os_state::StateHandle;
use zksync_os_types::ZksyncOsEncode;

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
    /// notifications about tree version to allow the batcher to wait until the tree is ready
    tree_version_watch: watch::Receiver<u64>,
    persistent_tree: MerkleTree<RocksDBWrapper>,
    state_handle: StateHandle,
    bin_path: &'static str,
    maximum_in_flight_blocks: usize,
}

impl Batcher {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        block_receiver: tokio::sync::mpsc::Receiver<(BatchOutput, ReplayRecord)>,
        batch_sender: tokio::sync::mpsc::Sender<BatchJob>,
        // handled by l1-sender
        commit_batch_info_sender: Option<L1SenderHandle>,
        tree_version_watch: watch::Receiver<u64>,
        state_handle: StateHandle,
        persistent_tree: MerkleTree<RocksDBWrapper>,

        enable_logging: bool,
        maximum_in_flight_blocks: usize,
    ) -> Self {
        // Use path relative to crate's Cargo.toml to ensure consistent pathing in different contexts
        let bin_path = if enable_logging {
            concat!(env!("CARGO_MANIFEST_DIR"), "/../../app_logging_enabled.bin")
        } else {
            concat!(env!("CARGO_MANIFEST_DIR"), "/../../app.bin")
        };
        Self {
            block_receiver,
            state_handle,
            batch_sender,
            tree_version_watch,
            persistent_tree,
            commit_batch_info_sender,
            bin_path,
            maximum_in_flight_blocks,
        }
    }

    /// Works on multiple blocks in parallel. May use up to [Self::maximum_in_flight_blocks] threads but
    /// will only take up new work once the oldest block finishes processing.
    pub async fn run_loop(self) -> anyhow::Result<()> {
        ReceiverStream::new(self.block_receiver)
            .then(|(batch_output, replay_record)| {
                // delay the stream so the necessary tree version is available
                let mut tree_version = self.tree_version_watch.clone();
                async move {
                    tree_version
                        .wait_for(|&tree_version| {
                            tree_version >= replay_record.block_context.block_number - 1
                        })
                        .await
                        .unwrap();
                    (batch_output, replay_record)
                }
            })
            .map(|(batch_output, replay_record)| {
                BATCHER_METRICS
                    .current_block_number
                    .set(replay_record.block_context.block_number);
                let block_number = replay_record.block_context.block_number;
                tracing::debug!(
                    "Batcher started processing block {} with {} transactions",
                    block_number,
                    replay_record.transactions.len(),
                );

                let persistent_tree = self.persistent_tree.clone();
                let state_handle = self.state_handle.clone();
                tokio::task::spawn_blocking(move || {
                    (
                        compute_prover_input(
                            &replay_record,
                            state_handle,
                            persistent_tree.clone(),
                            self.bin_path,
                        ),
                        batch_output,
                        replay_record,
                    )
                })
            })
            .buffered(self.maximum_in_flight_blocks)
            .map_err(|e| anyhow::anyhow!(e))
            .and_then(|(a, b, replay_record)| {
                // delay the stream so the necessary tree version is available
                let mut tree_version = self.tree_version_watch.clone();
                async move {
                    tree_version
                        .wait_for(|&tree_version| {
                            tree_version >= replay_record.block_context.block_number
                        })
                        .await?;
                    Ok((a, b, replay_record))
                }
            })
            .try_for_each(async |(prover_input, batch_output, replay_record)| {
                let block_number = replay_record.block_context.block_number;

                let (root_hash, leaf_count) = self
                    .persistent_tree
                    .root_info(block_number)
                    .unwrap()
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

                if let Some(l1) = &self.commit_batch_info_sender {
                    l1.commit(commit_batch_info.clone()).await?;
                }
                self.batch_sender
                    .send(BatchJob {
                        block_number,
                        prover_input,
                        commit_batch_info,
                    })
                    .await?;

                Ok(())
            })
            .await
    }
}

fn compute_prover_input(
    replay_record: &ReplayRecord,
    state_handle: StateHandle,
    persistent_tree: MerkleTree<RocksDBWrapper>,
    bin_path: &'static str,
) -> Vec<u32> {
    let block_number = replay_record.block_context.block_number;
    let previous_block = block_number - 1;

    let (root_hash, leaf_count) = persistent_tree.root_info(previous_block).unwrap().unwrap();
    let initial_storage_commitment = StorageCommitment {
        root: fixed_bytes_to_bytes32(root_hash),
        next_free_slot: leaf_count,
    };

    let state_view = state_handle.state_view_at_block(block_number).unwrap();

    let transactions = replay_record
        .transactions
        .iter()
        .map(|tx| tx.clone().encode())
        .collect::<VecDeque<_>>();
    let list_source = TxListSource { transactions };

    let tree_view = MerkleTreeVersion {
        tree: persistent_tree.clone(),
        block: previous_block,
    };

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
