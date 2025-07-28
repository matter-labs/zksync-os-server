use crate::metrics::GENERAL_METRICS;
use crate::model::batches::{BatchEnvelope, ProverInput};
use crate::model::blocks::ReplayRecord;
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
use zk_os_forward_system::run::{StorageCommitment, generate_proof_input};
use zksync_os_merkle_tree::{
    MerkleTreeForReading, MerkleTreeVersion, RocksDBWrapper, fixed_bytes_to_bytes32,
};
use zksync_os_state::StateHandle;
use zksync_os_types::ZksyncOsEncode;

#[derive(Debug)]
pub struct ProverInputGeneratorBatchData {
    pub replay_records: Vec<ReplayRecord>,
    pub previous_block_timestamp: u64,
}

/// This component generates prover input from batch replay data
pub struct ProverInputGenerator {
    // == config ==
    bin_path: &'static str,
    maximum_in_flight_blocks: usize,

    // == plumbing ==
    // inbound
    batch_replay_data_receiver: Receiver<BatchEnvelope<ProverInputGeneratorBatchData>>,

    // outbound
    batch_for_proving_sender: Sender<BatchEnvelope<ProverInput>>,

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
        batch_replay_data_receiver: Receiver<BatchEnvelope<ProverInputGeneratorBatchData>>,
        batch_for_proving_sender: Sender<BatchEnvelope<ProverInput>>,

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
            batch_replay_data_receiver,
            batch_for_proving_sender,
            persistent_tree,
            bin_path,
            maximum_in_flight_blocks,
            state_handle,
        }
    }

    /// Works on multiple blocks in parallel. May use up to [Self::maximum_in_flight_blocks] threads but
    /// will only take up new work once the oldest block finishes processing.
    pub async fn run_loop(self) -> Result<()> {
        ReceiverStream::new(self.batch_replay_data_receiver)
            // wait for tree to have processed block for each replay record
            .then(|batch_replay_data| {
                let tree = self.persistent_tree.clone();
                async move {
                    let mut processed_replays = Vec::new();
                    // todo: now this loop is only doing one iteration (batch_replay_data.block_replays.len() == 1)
                    // we can change approach (e.g. don't have a separate stream step for tree)
                    // note: in fact tree is guaranteed to be available here
                    // since this batch was already processed by batcher that also needs/waits for the tree

                    let iter = batch_replay_data.data.replay_records.into_iter().scan(
                        batch_replay_data.data.previous_block_timestamp,
                        |previous_block_timestamp, replay_record| {
                            let next_previous_block_timestamp =
                                replay_record.block_context.timestamp;
                            let item = Some((*previous_block_timestamp, replay_record));
                            *previous_block_timestamp = next_previous_block_timestamp;
                            item
                        },
                    );

                    for (previous_block_timestamp, replay_record) in iter {
                        let tree = tree
                            .clone()
                            .get_at_block(replay_record.block_context.block_number - 1)
                            .await;
                        processed_replays.push((tree, previous_block_timestamp, replay_record));
                    }

                    (
                        batch_replay_data.batch,
                        batch_replay_data.trace,
                        processed_replays,
                    )
                }
            })
            // generate prover input. Use up to `Self::maximum_in_flight_blocks` threads
            .map(|(batch_metadata, trace, processed_replays)| {
                // For now, we only have one block per batch
                assert_eq!(processed_replays.len(), 1);
                let (tree, previous_block_timestamp, replay_record) =
                    processed_replays.into_iter().next().unwrap();
                let block_number = replay_record.block_context.block_number;

                tracing::debug!(
                    "ProverInputGenerator started processing block {} with {} transactions",
                    block_number,
                    replay_record.transactions.len(),
                );

                let state_handle = self.state_handle.clone();
                tokio::task::spawn_blocking(move || {
                    (
                        batch_metadata,
                        trace,
                        compute_prover_input(
                            &replay_record,
                            state_handle,
                            tree,
                            self.bin_path,
                            previous_block_timestamp,
                        ),
                    )
                })
            })
            // note on parallelism: currently we process multiple blocks/batches in parallel,
            // when we have proper batching, we can process multiple blocks within one batch in paralle,
            // but not have multiple batches in parallel
            // still, we should be able to add cross-batch parallelism later on
            .buffered(self.maximum_in_flight_blocks)
            .map_err(|e| anyhow::anyhow!(e))
            .try_for_each(|(batch_metadata, trace, prover_input)| async {
                GENERAL_METRICS.block_number[&"prover_input_generator"]
                    .set(batch_metadata.commit_batch_info.batch_number);

                let output_envelope = BatchEnvelope {
                    batch: batch_metadata,
                    data: prover_input,
                    trace: trace.with_stage("prover_input_generated"),
                };

                self.batch_for_proving_sender.send(output_envelope).await?;

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
    let batch_number = replay_record.block_context.block_number;

    let (root_hash, leaf_count) = tree_view.root_info().unwrap();
    let initial_storage_commitment = StorageCommitment {
        root: fixed_bytes_to_bytes32(root_hash),
        next_free_slot: leaf_count,
    };

    let state_view = state_handle.state_view_at_block(batch_number - 1).unwrap();

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
        batch_number,
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
