use anyhow::Result;
use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::{StreamExt, TryStreamExt};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use vise::{Buckets, Histogram, LabeledFamily, Metrics, Unit};
use zk_ee::common_structs::DACommitmentScheme;
use zksync_os_batch_types::BlockMerkleTreeData;
use zksync_os_interface::traits::TxListSource;
use zksync_os_interface::types::BlockOutput;
use zksync_os_l1_sender::batcher_model::ProverInput;
use zksync_os_merkle_tree::{MerkleTreeVersion, RocksDBWrapper, fixed_bytes_to_bytes32};
use zksync_os_observability::{ComponentStateReporter, GenericComponentState};
use zksync_os_pipeline::{PeekableReceiver, PipelineComponent};
use zksync_os_storage_api::{ReadStateHistory, ReplayRecord};
use zksync_os_types::{ExecutionVersion, ProvingVersion, PubdataMode, ZksyncOsEncode};

/// This component generates prover input from batch replay data
pub struct ProverInputGenerator<ReadState> {
    pub enable_logging: bool,
    pub maximum_in_flight_blocks: usize,
    pub app_bin_base_path: PathBuf,
    pub read_state: ReadState,
    pub pubdata_mode: PubdataMode,
}

#[async_trait]
impl<ReadState: ReadStateHistory + Clone + Send + 'static> PipelineComponent
    for ProverInputGenerator<ReadState>
{
    type Input = (BlockOutput, ReplayRecord, BlockMerkleTreeData);
    type Output = (BlockOutput, ReplayRecord, ProverInput, BlockMerkleTreeData);

    const NAME: &'static str = "prover_input_generator";
    const OUTPUT_BUFFER_SIZE: usize = 5;

    /// Works on multiple blocks in parallel. May use up to [Self::maximum_in_flight_blocks] threads but
    /// will only take up new work once the oldest block finishes processing.
    async fn run(
        self,
        input: PeekableReceiver<Self::Input>,
        output: mpsc::Sender<Self::Output>,
    ) -> Result<()> {
        let latency_tracker = ComponentStateReporter::global().handle_for(
            "prover_input_generator",
            GenericComponentState::ProcessingOrWaitingRecv,
        );

        let read_state = self.read_state;
        let pubdata_mode = self.pubdata_mode;
        let enable_logging = self.enable_logging;
        let app_bin_base_path = self.app_bin_base_path;
        let maximum_in_flight_blocks = self.maximum_in_flight_blocks;

        let mut input = input.into_inner();
        // We want to process the first item separately as it involves some heavy trusted-setup-related precomputation.
        let Some(first_item) = input.recv().await else {
            return Ok(());
        };
        // We create two streams: one for the first item, and one for the rest of the input.
        let streams: Vec<BoxStream<Self::Input>> = vec![
            futures::stream::once(async { first_item }).boxed(),
            ReceiverStream::new(input).boxed(),
        ];
        // Streams are processed sequentially but in the same way.
        for s in streams {
            // Generates prover input. Uses up to `maximum_in_flight_blocks` threads
            s.map(|(block_output, replay_record, tree)| {
                let block_number = replay_record.block_context.block_number;

                tracing::debug!(
                    "ProverInputGenerator started processing block {} with {} transactions",
                    block_number,
                    replay_record.transactions.len(),
                );
                let read_state_clone = read_state.clone();
                let app_bin_base_path_clone = app_bin_base_path.clone();

                // we need to adapt pubdata mode depending on protocol version, to ensure automatic DA mode change during v30 upgrade
                let da_commitment_scheme = pubdata_mode
                    .adapt_for_protocol_version(&replay_record.protocol_version)
                    .da_commitment_scheme();
                let da_commitment_scheme = (da_commitment_scheme as u8)
                    .try_into()
                    .expect("Failed to convert DA commitment scheme");

                tokio::task::spawn_blocking(move || {
                    let prover_input = compute_prover_input(
                        &replay_record,
                        read_state_clone,
                        tree.block_start.clone(),
                        da_commitment_scheme,
                        app_bin_base_path_clone,
                        enable_logging,
                    );
                    (block_output, replay_record, prover_input, tree)
                })
            })
            .buffered(maximum_in_flight_blocks)
            .map_err(|e| anyhow::anyhow!(e))
            .try_for_each(|(block_output, replay_record, prover_input, tree)| async {
                latency_tracker.enter_state(GenericComponentState::WaitingSend);
                tracing::debug!(
                    block_number = block_output.header.number,
                    "sending block with prover input to batcher",
                );
                output
                    .send((block_output, replay_record, prover_input, tree))
                    .await?;
                latency_tracker.enter_state(GenericComponentState::ProcessingOrWaitingRecv);
                Ok(())
            })
            .await?;
        }
        Ok(())
    }
}

fn compute_prover_input(
    replay_record: &ReplayRecord,
    state_handle: impl ReadStateHistory,
    tree_view: MerkleTreeVersion<RocksDBWrapper>,
    da_commitment_scheme: DACommitmentScheme,
    app_bin_base_path: PathBuf,
    enable_logging: bool,
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
    let forward_run_execution_version =
        ExecutionVersion::try_from(replay_record.block_context.execution_version)
            .expect("Must be valid execution as set by the server");
    let prover_input =
        match ProvingVersion::from_forward_run_execution_version(forward_run_execution_version) {
            ProvingVersion::V1 | ProvingVersion::V2 | ProvingVersion::V3 => {
                panic!(
                    "computing prover input for batch with prover version v1-v3 is not supported"
                );
            }
            ProvingVersion::V4 => {
                use zk_ee_0_1_0::{
                    common_structs::ProofData,
                    system::metadata::zk_metadata::BlockMetadataFromOracle,
                };
                use zk_os_forward_system_0_1_0::run::{
                    StorageCommitment, convert::FromInterface, generate_proof_input,
                };

                let initial_storage_commitment = StorageCommitment {
                    root: fixed_bytes_to_bytes32(root_hash).as_u8_array().into(),
                    next_free_slot: leaf_count,
                };

                let list_source = TxListSource { transactions };

                let bin_path = if enable_logging {
                    zksync_os_multivm::apps::v4::singleblock_batch_logging_enabled_path(
                        &app_bin_base_path,
                    )
                } else {
                    zksync_os_multivm::apps::v4::singleblock_batch_path(&app_bin_base_path)
                };

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
            ProvingVersion::V5 => {
                use zk_ee::{
                    common_structs::ProofData,
                    system::metadata::zk_metadata::BlockMetadataFromOracle,
                };
                use zk_os_forward_system::run::{
                    StorageCommitment, convert::FromInterface, generate_proof_input,
                };

                let initial_storage_commitment = StorageCommitment {
                    root: fixed_bytes_to_bytes32(root_hash).as_u8_array().into(),
                    next_free_slot: leaf_count,
                };

                let list_source = TxListSource { transactions };

                let bin_path = if enable_logging {
                    zksync_os_multivm::apps::v5::singleblock_batch_logging_enabled_path(
                        &app_bin_base_path,
                    )
                } else {
                    zksync_os_multivm::apps::v5::singleblock_batch_path(&app_bin_base_path)
                };

                generate_proof_input(
                    bin_path,
                    BlockMetadataFromOracle::from_interface(replay_record.block_context),
                    ProofData {
                        state_root_view: initial_storage_commitment,
                        last_block_timestamp: replay_record.previous_block_timestamp,
                    },
                    da_commitment_scheme,
                    tree_view,
                    state_view,
                    list_source,
                )
                .expect("proof gen failed")
            }
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
