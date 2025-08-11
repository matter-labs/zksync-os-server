use crate::metrics::GENERAL_METRICS;
use anyhow::Result;
use futures::{StreamExt, TryStreamExt};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio_stream::wrappers::ReceiverStream;
use vise::{Buckets, Histogram, LabeledFamily, Metrics, Unit};
use zk_ee::common_structs::ProofData;
use zk_os_forward_system::run::test_impl::TxListSource;
use zk_os_forward_system::run::{BlockOutput, StorageCommitment, generate_proof_input};
use zksync_os_l1_sender::model::ProverInput;
use zksync_os_merkle_tree::{
    MerkleTreeForReading, MerkleTreeVersion, RocksDBWrapper, fixed_bytes_to_bytes32,
};
use zksync_os_state::StateHandle;
use zksync_os_storage_api::ReplayRecord;
use zksync_os_types::ZksyncOsEncode;

/// This component generates prover input from batch replay data
pub struct ProverInputGenerator {
    // == config ==
    bin_path: &'static str,
    maximum_in_flight_blocks: usize,

    // == plumbing ==
    // inbound
    block_receiver: Receiver<(BlockOutput, ReplayRecord)>,

    // outbound
    blocks_for_batcher_sender: Sender<(BlockOutput, ReplayRecord, ProverInput)>,

    // dependencies
    persistent_tree: MerkleTreeForReading<RocksDBWrapper>,
    state_handle: StateHandle,
}

impl ProverInputGenerator {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        // == config ==
        enable_logging: bool,
        maximum_in_flight_blocks: usize,

        // == plumbing ==
        block_receiver: Receiver<(BlockOutput, ReplayRecord)>,
        blocks_for_batcher_sender: Sender<(BlockOutput, ReplayRecord, ProverInput)>,

        // == dependencies ==
        persistent_tree: MerkleTreeForReading<RocksDBWrapper>,
        state_handle: StateHandle,
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
            blocks_for_batcher_sender,
            persistent_tree,
            bin_path,
            maximum_in_flight_blocks,
            state_handle,
        }
    }

    /// Works on multiple blocks in parallel. May use up to [Self::maximum_in_flight_blocks] threads but
    /// will only take up new work once the oldest block finishes processing.
    pub async fn run_loop(self) -> Result<()> {
        ReceiverStream::new(self.block_receiver)
            // wait for tree to have processed block for each replay record
            .then(|(block_output, replay_record)| {
                let tree = self.persistent_tree.clone();
                async move {
                    let tree = tree
                        .clone()
                        .get_at_block(replay_record.block_context.block_number - 1)
                        .await;
                    (block_output, replay_record, tree)
                }
            })
            // generate prover input. Use up to `Self::maximum_in_flight_blocks` threads
            .map(|(block_output, replay_record, tree)| {
                let block_number = replay_record.block_context.block_number;

                tracing::debug!(
                    "ProverInputGenerator started processing block {} with {} transactions",
                    block_number,
                    replay_record.transactions.len(),
                );

                let state_handle = self.state_handle.clone();

                tokio::task::spawn_blocking(move || {
                    let prover_input = compute_prover_input(
                        &replay_record,
                        state_handle,
                        tree,
                        self.bin_path,
                        replay_record.previous_block_timestamp,
                    );
                    (block_output, replay_record, prover_input)
                })
            })
            .buffered(self.maximum_in_flight_blocks)
            .map_err(|e| anyhow::anyhow!(e))
            .try_for_each(|(block_output, replay_record, prover_input)| async {
                GENERAL_METRICS.block_number[&"prover_input_generator"]
                    .set(block_output.header.number);

                self.blocks_for_batcher_sender
                    .send((block_output, replay_record, prover_input))
                    .await?;

                Ok(())
            })
            .await
    }
}

fn compute_prover_input(
    replay_record: &ReplayRecord,
    state_handle: StateHandle,
    tree_view: MerkleTreeVersion<RocksDBWrapper>,
    bin_path: &'static str,
    last_block_timestamp: u64,
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
        PROVER_INPUT_GENERATOR_METRICS.prover_input_generation[&"prover_input_generation"].start();
    let prover_input = generate_proof_input(
        PathBuf::from(bin_path),
        replay_record.block_context,
        ProofData {
            state_root_view: initial_storage_commitment,
            last_block_timestamp,
        },
        tree_view,
        state_view,
        list_source,
    )
    .expect("proof gen failed");

    let latency = prover_input_generation_latency.observe();

    tracing::info!(
        block_number,
        "Completed prover input computation in {:?}.",
        latency
    );

    prover_input
}

const LATENCIES_FAST: Buckets = Buckets::exponential(0.0000001..=1.0, 2.0);
#[derive(Debug, Metrics)]
#[metrics(prefix = "prover_input_generator")]
pub struct ProverInputGeneratorMetrics {
    #[metrics(unit = Unit::Seconds, labels = ["stage"], buckets = LATENCIES_FAST)]
    pub prover_input_generation: LabeledFamily<&'static str, Histogram<Duration>>,
}

#[vise::register]
pub(crate) static PROVER_INPUT_GENERATOR_METRICS: vise::Global<ProverInputGeneratorMetrics> =
    vise::Global::new();
