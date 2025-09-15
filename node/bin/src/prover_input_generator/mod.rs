use anyhow::Result;
use futures::future::Either;
use futures::{StreamExt, TryFutureExt, TryStreamExt};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio_stream::wrappers::ReceiverStream;
use vise::{Buckets, Histogram, LabeledFamily, Metrics, Unit};
use zksync_os_interface::types::BlockOutput;
use zksync_os_l1_sender::batcher_model::ProverInput;
use zksync_os_merkle_tree::{
    MerkleTreeForReading, MerkleTreeVersion, RocksDBWrapper, fixed_bytes_to_bytes32,
};
use zksync_os_multivm::apps::create_temp_file;
use zksync_os_observability::{ComponentStateReporter, GenericComponentState};
use zksync_os_storage_api::{ReadStateHistory, ReplayRecord};
use zksync_os_types::ZksyncOsEncode;

/// This component generates prover input from batch replay data
pub struct ProverInputGenerator<ReadState> {
    // == state ==
    bin_bytes: &'static [u8],

    // == config ==
    maximum_in_flight_blocks: usize,
    first_block_to_process: u64,

    // == plumbing ==
    // inbound
    block_receiver: Receiver<(BlockOutput, ReplayRecord)>,

    // outbound
    blocks_for_batcher_sender: Sender<(BlockOutput, ReplayRecord, ProverInput)>,

    // dependencies
    persistent_tree: MerkleTreeForReading<RocksDBWrapper>,
    read_state: ReadState,
}

impl<ReadState: ReadStateHistory + Clone> ProverInputGenerator<ReadState> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        // == config ==
        enable_logging: bool,
        maximum_in_flight_blocks: usize,
        first_block_to_process: u64,

        // == plumbing ==
        block_receiver: Receiver<(BlockOutput, ReplayRecord)>,
        blocks_for_batcher_sender: Sender<(BlockOutput, ReplayRecord, ProverInput)>,

        // == dependencies ==
        persistent_tree: MerkleTreeForReading<RocksDBWrapper>,
        read_state: ReadState,
    ) -> Self {
        // Use path relative to crate's Cargo.toml to ensure consistent pathing in different contexts
        let bin_bytes = if enable_logging {
            zksync_os_multivm::apps::v1::SERVER_APP_LOGGING_ENABLED
        } else {
            zksync_os_multivm::apps::v1::SERVER_APP
        };

        Self {
            block_receiver,
            blocks_for_batcher_sender,
            first_block_to_process,
            persistent_tree,
            bin_bytes,
            maximum_in_flight_blocks,
            read_state,
        }
    }

    /// Works on multiple blocks in parallel. May use up to [Self::maximum_in_flight_blocks] threads but
    /// will only take up new work once the oldest block finishes processing.
    pub async fn run_loop(self) -> Result<()> {
        let latency_tracker = ComponentStateReporter::global().handle_for(
            "prover_input_generator",
            GenericComponentState::ProcessingOrWaitingRecv,
        );
        ReceiverStream::new(self.block_receiver)
            // skip the blocks that were already committed
            .skip_while(|(_, replay_record)| {
                let block_number = replay_record.block_context.block_number;
                async move {
                    if block_number < self.first_block_to_process {
                        tracing::debug!(
                            "Skipping block {} as it's below the first block to process {}",
                            block_number,
                            self.first_block_to_process
                        );
                        true
                    } else {
                        false
                    }
                }
            })
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
                let read_state = self.read_state.clone();

                let file = match create_temp_file(self.bin_bytes) {
                    Ok(f) => f,
                    Err(e) => {
                        return Either::Left(futures::future::err::<
                            (BlockOutput, ReplayRecord, ProverInput),
                            _,
                        >(anyhow::anyhow!(e)));
                    }
                };

                Either::Right(
                    tokio::task::spawn_blocking(move || {
                        let prover_input = compute_prover_input(
                            &replay_record,
                            read_state,
                            tree,
                            file.path().to_path_buf(),
                        );
                        (block_output, replay_record, prover_input)
                    })
                    .map_err(|e| anyhow::anyhow!(e)),
                )
            })
            .buffered(self.maximum_in_flight_blocks)
            .try_for_each(|(block_output, replay_record, prover_input)| async {
                latency_tracker.enter_state(GenericComponentState::WaitingSend);
                tracing::debug!(
                    block_number = block_output.header.number,
                    "sending block with prover input to batcher",
                );
                self.blocks_for_batcher_sender
                    .send((block_output, replay_record, prover_input))
                    .await?;
                latency_tracker.enter_state(GenericComponentState::ProcessingOrWaitingRecv);
                Ok(())
            })
            .await
    }
}

fn compute_prover_input(
    replay_record: &ReplayRecord,
    state_handle: impl ReadStateHistory,
    tree_view: MerkleTreeVersion<RocksDBWrapper>,
    bin_path: PathBuf,
) -> Vec<u32> {
    let block_number = replay_record.block_context.block_number;
    let state_view = state_handle.state_view_at(block_number - 1).unwrap();
    let (root_hash, leaf_count) = tree_view.root_info().unwrap();
    let transactions = replay_record
        .transactions
        .iter()
        .map(|tx| tx.clone().encode())
        .collect::<VecDeque<_>>();

    let prover_input_generation_latency =
        PROVER_INPUT_GENERATOR_METRICS.prover_input_generation[&"prover_input_generation"].start();
    let prover_input = match replay_record.block_context.protocol_version {
        1 => {
            use zk_ee::{common_structs::ProofData, system::metadata::BlockMetadataFromOracle};
            use zk_os_forward_system::run::{
                StorageCommitment, convert::FromInterface, generate_proof_input,
                test_impl::TxListSource,
            };

            let initial_storage_commitment = StorageCommitment {
                root: fixed_bytes_to_bytes32(root_hash).as_u8_array().into(),
                next_free_slot: leaf_count,
            };

            let list_source = TxListSource { transactions };

            generate_proof_input(
                bin_path,
                BlockMetadataFromOracle::from_interface(replay_record.block_context),
                ProofData {
                    state_root_view: initial_storage_commitment,
                    last_block_timestamp: replay_record.previous_block_timestamp,
                },
                tree_view,
                state_view,
                list_source,
            )
            .expect("proof gen failed")
        }
        v => panic!("Unsupported protocol version: {v}"),
    };
    let latency = prover_input_generation_latency.observe();

    tracing::info!(
        block_number,
        "Completed prover input computation in {:?}.",
        latency
    );

    prover_input
}

const LATENCIES_FAST: Buckets = Buckets::exponential(0.001..=30.0, 2.0);
#[derive(Debug, Metrics)]
#[metrics(prefix = "prover_input_generator")]
pub struct ProverInputGeneratorMetrics {
    #[metrics(unit = Unit::Seconds, labels = ["stage"], buckets = LATENCIES_FAST)]
    pub prover_input_generation: LabeledFamily<&'static str, Histogram<Duration>>,
}

#[vise::register]
pub(crate) static PROVER_INPUT_GENERATOR_METRICS: vise::Global<ProverInputGeneratorMetrics> =
    vise::Global::new();
