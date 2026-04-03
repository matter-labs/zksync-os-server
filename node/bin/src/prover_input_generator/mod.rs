use anyhow::Result;
use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::FuturesOrdered;
use reth_tasks::Runtime;
use std::collections::VecDeque;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use vise::{Buckets, Histogram, LabeledFamily, Metrics, Unit};
use zksync_os_batch_types::BlockMerkleTreeData;
use zksync_os_contract_interface::models::DACommitmentScheme;
use zksync_os_interface::traits::TxListSource;
use zksync_os_interface::types::BlockOutput;
use zksync_os_l1_sender::batcher_model::ProverInput;
use zksync_os_merkle_tree::{MerkleTreeVersion, RocksDBWrapper, fixed_bytes_to_bytes32};
use zksync_os_observability::{ComponentStateReporter, GenericComponentState};
use zksync_os_pipeline::PeekableReceiver;
use zksync_os_pipeline::PipelineComponent;
use zksync_os_storage_api::{ReadStateHistory, ReplayRecord};
use zksync_os_types::{ProvingVersion, PubdataMode, ZksyncOsEncode};

/// This component generates prover input from batch replay data.
///
/// When `disabled` is `true` the component acts as a passthrough: it forwards each block
/// unchanged but sets `ProverInput::Fake` instead of computing real witness data.
/// This is only valid when both FRI and SNARK provers are faked.
pub struct ProverInputGenerator<ReadState> {
    pub enable_logging: bool,
    pub maximum_in_flight_blocks: usize,
    pub read_state: ReadState,
    pub pubdata_mode: PubdataMode,
    pub runtime: Runtime,
    /// When true, skip all computation and emit `ProverInput::Fake` for every block.
    pub disabled: bool,
}

#[async_trait]
impl<ReadState: ReadStateHistory + Clone + Send + 'static> PipelineComponent
    for ProverInputGenerator<ReadState>
{
    type Input = (BlockOutput, ReplayRecord, BlockMerkleTreeData);
    type Output = (BlockOutput, ReplayRecord, ProverInput, BlockMerkleTreeData);

    const NAME: &'static str = "prover_input_generator";
    const OUTPUT_BUFFER_SIZE: usize = 5;

    /// Works on multiple blocks in parallel, up to [Self::maximum_in_flight_blocks].
    /// Each computation runs on the blocking pool and is tracked as a graceful task so
    /// the RocksDB tree lock held by [BlockMerkleTreeData] is always released before
    /// [graceful_shutdown_with_timeout] returns.
    async fn run(
        self,
        mut input: PeekableReceiver<Self::Input>,
        output: mpsc::Sender<Self::Output>,
    ) -> Result<()> {
        if self.disabled {
            tracing::info!(
                "ProverInputGenerator is disabled — passing through blocks with ProverInput::Fake"
            );
            while let Some((block_output, replay_record, tree)) = input.recv().await {
                output
                    .send((block_output, replay_record, ProverInput::Fake, tree))
                    .await?;
            }
            return Ok(());
        }

        let latency_tracker = ComponentStateReporter::global().handle_for(
            "prover_input_generator",
            GenericComponentState::ProcessingOrWaitingRecv,
        );

        // Process the first item alone — it involves heavy trusted-setup precomputation
        // and we want it isolated before concurrent processing starts.
        let first_item = match input.recv().await {
            Some(item) => item,
            None => return Ok(()),
        };
        let result = self.spawn_computation(first_item).await?;
        latency_tracker.enter_state(GenericComponentState::WaitingSend);
        tracing::debug!(
            block_number = result.0.header.number,
            "sending block with prover input to batcher",
        );
        output.send(result).await?;
        latency_tracker.enter_state(GenericComponentState::ProcessingOrWaitingRecv);

        // Process remaining items with up to `maximum_in_flight_blocks` in parallel.
        // Results are delivered in arrival order via FuturesOrdered.
        let mut pending: FuturesOrdered<
            oneshot::Receiver<(BlockOutput, ReplayRecord, ProverInput, BlockMerkleTreeData)>,
        > = FuturesOrdered::new();
        let mut input_done = false;

        loop {
            if input_done && pending.is_empty() {
                break;
            }

            tokio::select! {
                maybe_item = input.recv(),
                    if !input_done && pending.len() < self.maximum_in_flight_blocks =>
                {
                    match maybe_item {
                        Some(item) => pending.push_back(self.spawn_computation(item)),
                        None => input_done = true,
                    }
                }
                Some(result) = pending.next(), if !pending.is_empty() => {
                    let item = result.map_err(|_| anyhow::anyhow!("prover input computation task dropped sender"))?;
                    latency_tracker.enter_state(GenericComponentState::WaitingSend);
                    tracing::debug!(
                        block_number = item.0.header.number,
                        "sending block with prover input to batcher",
                    );
                    output.send(item).await?;
                    latency_tracker.enter_state(GenericComponentState::ProcessingOrWaitingRecv);
                }
            }
        }

        Ok(())
    }
}

impl<ReadState: ReadStateHistory + Clone + Send + 'static> ProverInputGenerator<ReadState> {
    /// Submits one block's prover-input computation to the blocking CPU pool and returns
    /// a receiver for the result. The computation is tracked as a graceful task so its
    /// [BlockMerkleTreeData] (holding the tree RocksDB lock) is guaranteed to be dropped
    /// before [graceful_shutdown_with_timeout] returns.
    fn spawn_computation(
        &self,
        (block_output, replay_record, tree): (BlockOutput, ReplayRecord, BlockMerkleTreeData),
    ) -> oneshot::Receiver<(BlockOutput, ReplayRecord, ProverInput, BlockMerkleTreeData)> {
        let (result_tx, result_rx) = oneshot::channel();
        let read_state = self.read_state.clone();
        let enable_logging = self.enable_logging;
        let da_commitment_scheme = self
            .pubdata_mode
            .adapt_for_protocol_version(&replay_record.protocol_version)
            .da_commitment_scheme();
        let block_number = replay_record.block_context.block_number;
        tracing::debug!(
            block_number,
            "ProverInputGenerator started processing block {} with {} transactions",
            block_number,
            replay_record.transactions.len(),
        );
        let mut handle = tokio::task::spawn_blocking(move || {
            let prover_input = ProverInput::Real(compute_prover_input(
                &replay_record,
                read_state,
                tree.block_start.clone(),
                da_commitment_scheme,
                enable_logging,
            ));
            (block_output, replay_record, prover_input, tree)
        });
        self.runtime.spawn_critical_with_graceful_shutdown_signal(
            "prover input computation",
            |shutdown| async move {
                tokio::select! {
                    Ok(result) = &mut handle => {
                        let _ = result_tx.send(result);
                    }
                    _guard = shutdown => {
                        // Wait for CPU task to finish while holding shutdown guard. This blocks
                        // shutdown until prover input generation task finishes and frees up tree DB.
                        let _ = handle.await;
                    }
                }
            },
        );

        result_rx
    }
}

fn compute_prover_input(
    replay_record: &ReplayRecord,
    state_handle: impl ReadStateHistory,
    tree_view: MerkleTreeVersion<RocksDBWrapper>,
    da_commitment_scheme: DACommitmentScheme,
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
    let proving_version = ProvingVersion::try_from(replay_record.protocol_version.clone())
        .expect("invalid protocol version");
    let prover_input = match proving_version {
        ProvingVersion::V1
        | ProvingVersion::V2
        | ProvingVersion::V3
        | ProvingVersion::V4
        | ProvingVersion::V5 => {
            panic!("computing prover input for batch with prover version v1-v5 is not supported");
        }
        ProvingVersion::V6 => {
            use zk_ee::{
                common_structs::ProofData, system::metadata::zk_metadata::BlockMetadataFromOracle,
            };
            use zk_os_forward_system::run::{
                StorageCommitment, convert::FromInterface, generate_proof_input_from_bytes,
            };

            let initial_storage_commitment = StorageCommitment {
                root: fixed_bytes_to_bytes32(root_hash).as_u8_array().into(),
                next_free_slot: leaf_count,
            };

            let list_source = TxListSource { transactions };

            let bin_bytes = if enable_logging {
                zksync_os_multivm::apps::v6::SINGLEBLOCK_BATCH_LOGGING_ENABLED
            } else {
                zksync_os_multivm::apps::v6::SINGLEBLOCK_BATCH_APP
            };

            let da_commitment_scheme = (da_commitment_scheme as u8)
                .try_into()
                .expect("Failed to convert DA commitment scheme");
            generate_proof_input_from_bytes(
                bin_bytes,
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
        ProvingVersion::V7 => {
            use zk_ee_dev::{
                common_structs::ProofData, system::metadata::zk_metadata::BlockMetadataFromOracle,
            };
            use zk_os_forward_system_dev::run::{
                StorageCommitment, convert::FromInterface, generate_proof_input_from_bytes,
            };

            let initial_storage_commitment = StorageCommitment {
                root: fixed_bytes_to_bytes32(root_hash).as_u8_array().into(),
                next_free_slot: leaf_count,
            };

            let list_source = TxListSource { transactions };

            let bin_bytes = if enable_logging {
                zksync_os_multivm::apps::v7::SINGLEBLOCK_BATCH_LOGGING_ENABLED
            } else {
                zksync_os_multivm::apps::v7::SINGLEBLOCK_BATCH_APP
            };

            let da_commitment_scheme = (da_commitment_scheme as u8)
                .try_into()
                .expect("Failed to convert DA commitment scheme");
            generate_proof_input_from_bytes(
                bin_bytes,
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
