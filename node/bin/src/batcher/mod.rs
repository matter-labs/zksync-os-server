use crate::batcher::seal_criteria::BatchInfoAccumulator;
use crate::config::BatcherConfig;
use alloy::consensus::BlobTransactionSidecar;
use alloy::primitives::Address;
use anyhow::Context;
use async_trait::async_trait;
use std::pin::Pin;
use tokio::sync::mpsc;
use tokio::time::{Instant, Sleep};
use tracing;
use zksync_os_batch_types::{BlockMerkleTreeData, DiscoveredCommittedBatch};
use zksync_os_contract_interface::models::StoredBatchInfo;
use zksync_os_interface::types::BlockOutput;
use zksync_os_l1_sender::batcher_metrics::BATCHER_METRICS;
use zksync_os_l1_sender::batcher_model::{
    BatchEnvelope, BatchForSigning, MissingSignature, ProverInput,
};
use zksync_os_l1_watcher::CommittedBatchProvider;
use zksync_os_merkle_tree::TreeBatchOutput;
use zksync_os_observability::{
    ComponentStateHandle, ComponentStateReporter, GenericComponentState,
};
use zksync_os_pipeline::{PeekableReceiver, PipelineComponent};
use zksync_os_storage_api::ReplayRecord;
use zksync_os_types::PubdataMode;

pub mod batch_builder;
mod seal_criteria;
pub mod util;

/// Set of fields to define batcher's behavior on startup (when to replay, when to produce, etc.)
pub struct BatcherStartupConfig {
    pub last_committed_batch: u64,
    pub last_executed_batch: u64,
    /// Last block number already known to this node. On startup, we'll replay all blocks until and including
    /// this - in other words, there will be no arbitrary delays until this block is passed through Batcher.
    /// We do not seal batches by timeout until this block is reached.
    /// This helps to avoid premature sealing due to timeout criterion, since for every tick of the
    /// timer the `should_seal_by_timeout` will often return `true`
    /// (because those blocks were produced during the previous run of the node - maybe some time ago)
    pub last_persisted_block: u64,
}

/// Batcher component - handles batching logic, receives blocks and prepares batch data
pub struct Batcher {
    pub startup_config: BatcherStartupConfig,
    pub chain_id: u64,
    pub sl_chain_id: u64,
    pub chain_address: Address,
    pub pubdata_limit_bytes: u64,
    pub batcher_config: BatcherConfig,
    pub pubdata_mode: PubdataMode,
    pub sidecar_sender: mpsc::Sender<BlobTransactionSidecar>,
    pub committed_batch_provider: CommittedBatchProvider,
}

#[async_trait]
impl PipelineComponent for Batcher {
    type Input = (BlockOutput, ReplayRecord, ProverInput, BlockMerkleTreeData);
    type Output = BatchEnvelope<ProverInput, MissingSignature>;

    const NAME: &'static str = "batcher";

    // The next component is `FriProvingPipelineStep` which contains an internal queue for FRI jobs.
    // We don't want to add additional buffers - as soon as the queue is full, we want to halt batching.
    const OUTPUT_BUFFER_SIZE: usize = 1;

    async fn run(
        mut self,
        mut input: PeekableReceiver<Self::Input>,
        output: mpsc::Sender<Self::Output>,
    ) -> anyhow::Result<()> {
        let latency_tracker = ComponentStateReporter::global()
            .handle_for("batcher", GenericComponentState::WaitingRecv);

        // We use last executed batch as the starting point. Next immediate batch we process will be
        // `last_executed_batch + 1`.
        let last_executed_batch = self
            .committed_batch_provider
            .get(self.startup_config.last_executed_batch)
            .with_context(|| {
                format!(
                    "last executed batch {} must have been discovered on L1",
                    self.startup_config.last_executed_batch
                )
            })?;
        let first_expected_block = last_executed_batch.last_block_number() + 1;
        let mut prev_batch_info = last_executed_batch.batch_info;

        // We might receive some blocks that belong to already executed batches. We can skip these
        // as there is no need to perform any L1 operations on them.
        loop {
            let next_block_number = input
                .peek_recv(|(_, replay_record, _, _)| replay_record.block_context.block_number)
                .await
                .context("batcher inbound channel unexpectedly closed")?;
            if next_block_number >= first_expected_block {
                break;
            }
            tracing::debug!(
                block_number = next_block_number,
                "skipping already executed block"
            );
            input
                .recv()
                .await
                .expect("impossible: missing an already peeked batch");
        }

        // Only used for metrics/logs
        let mut last_created_batch_at: Option<Instant> = None;

        loop {
            latency_tracker.enter_state(GenericComponentState::WaitingRecv);

            // Peek at the next block to decide whether to recreate or create anew.
            let next_block_number = input
                .peek_recv(|(_, replay_record, _, _)| replay_record.block_context.block_number)
                .await
                .context("batcher inbound channel unexpectedly closed")?;
            latency_tracker.enter_state(GenericComponentState::Processing);

            let batch_envelope;
            let recreated;
            if prev_batch_info.batch_number < self.startup_config.last_committed_batch {
                let committed_batch = self
                    .committed_batch_provider
                    .get(prev_batch_info.batch_number + 1)
                    .with_context(|| {
                        format!(
                            "committed batch {} must have been discovered on L1",
                            prev_batch_info.batch_number + 1
                        )
                    })?;
                // Validate that the existing batch's first block matches the next block in the stream
                anyhow::ensure!(
                    committed_batch.first_block_number() == next_block_number,
                    "Existing batch first block ({}) does not match next block in stream ({})",
                    committed_batch.first_block_number(),
                    next_block_number
                );

                batch_envelope = self
                    .recreate_existing_batch(
                        &mut input,
                        &latency_tracker,
                        &prev_batch_info,
                        committed_batch,
                    )
                    .await?;
                recreated = true;
            } else {
                batch_envelope = self
                    .create_batch(&mut input, &latency_tracker, &prev_batch_info)
                    .await?;
                recreated = false;
            };

            let time_since_last_batch =
                last_created_batch_at.map(|last_created_batch_at| last_created_batch_at.elapsed());
            if let Some(time_since_last_batch) = time_since_last_batch {
                BATCHER_METRICS
                    .time_since_last_batch
                    .observe(time_since_last_batch);
            }

            last_created_batch_at = Some(Instant::now());

            // Update prev_batch_info for the next iteration
            prev_batch_info = batch_envelope
                .batch
                .batch_info
                .clone()
                .into_stored(&batch_envelope.batch.protocol_version);

            BATCHER_METRICS
                .transactions_per_batch
                .observe(batch_envelope.batch.tx_count as u64);

            tracing::info!(
                batch_number = batch_envelope.batch_number(),
                batch_metadata = ?batch_envelope.batch,
                block_count = batch_envelope.batch.last_block_number - batch_envelope.batch.first_block_number + 1,
                new_state_commitment = ?batch_envelope.batch.batch_info.new_state_commitment,
                time_since_last_batch = ?time_since_last_batch,
                "Batch {}", if recreated { "recreated" } else { "created" }
            );

            tracing::debug!(
                batch_number = batch_envelope.batch_number(),
                da_commitment = ?batch_envelope.batch.batch_info.operator_da_input,
                "Batch da_input",
            );

            latency_tracker.enter_state(GenericComponentState::WaitingSend);
            if let Some(sidecar) = batch_envelope.batch.batch_info.blob_sidecar.clone() {
                self.sidecar_sender
                    .send(sidecar)
                    .await
                    .map_err(|e| anyhow::anyhow!("Failed to send sidecar: {e}"))?;
            }
            output
                .send(batch_envelope)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to send batch data: {e}"))?;
        }
    }
}

impl Batcher {
    async fn create_batch(
        &mut self,
        block_receiver: &mut PeekableReceiver<(
            BlockOutput,
            ReplayRecord,
            ProverInput,
            BlockMerkleTreeData,
        )>,
        latency_tracker: &ComponentStateHandle<GenericComponentState>,
        prev_batch_info: &StoredBatchInfo,
    ) -> anyhow::Result<BatchForSigning<ProverInput>> {
        // will be set to `Some` when we process the first block that the batch can be sealed after
        let mut deadline: Option<Pin<Box<Sleep>>> = None;

        let batch_number = prev_batch_info.batch_number + 1;
        let mut blocks: Vec<(BlockOutput, ReplayRecord, TreeBatchOutput, ProverInput)> = vec![];
        let mut accumulator = BatchInfoAccumulator::new(
            self.batcher_config.blocks_per_batch_limit,
            self.batcher_config.tx_per_batch_limit,
            self.pubdata_limit_bytes,
            self.batcher_config.interop_roots_per_batch_limit,
        );

        loop {
            latency_tracker.enter_state(GenericComponentState::WaitingRecv);
            tokio::select! {
                /* ---------- check for timeout ---------- */
                _ = async {
                    if let Some(d) = &mut deadline {
                        d.as_mut().await
                    }
                }, if deadline.is_some() => {
                    BATCHER_METRICS.seal_reason[&"timeout"].inc();
                    tracing::debug!(batch_number, "Timeout reached, sealing the batch.");
                    break;
                }

                /* ---------- collect blocks ---------- */
               should_seal = block_receiver.peek_recv(|(block_output, replay_record, _, _)| {
                    // determine if the block fits into the current batch
                    accumulator.clone().add(block_output, replay_record).should_seal()
                }) => {
                    latency_tracker.enter_state(GenericComponentState::Processing);
                    match should_seal {
                        Some(true) => {
                            // some of the limits was reached, start sealing the batch
                            break;
                        }
                        Some(false) => {
                            let Some((block_output, replay_record, prover_input, tree)) = block_receiver.pop_buffer() else {
                                anyhow::bail!("No block received in buffer after peeking")
                            };

                            let block_number = replay_record.block_context.block_number;

                            tracing::debug!(
                                batch_number,
                                block_number,
                                "Adding block to a pending batch."
                            );

                            let (root_hash, leaf_count) = tree.block_end.root_info()?;

                            let tree_output = TreeBatchOutput {
                                root_hash,
                                leaf_count,
                            };

                            // ---------- accumulate batch data ----------
                            accumulator.add(&block_output, &replay_record);

                            blocks.push((
                                block_output,
                                replay_record,
                                tree_output,
                                prover_input,
                            ));

                            // arm the timer after we process the block number that's more or equal
                            // than last persisted one - we don't want to seal on timeout if we know that there are still pending blocks in the inbound channel
                            if deadline.is_none() {
                                if block_number >= self.startup_config.last_persisted_block {
                                    deadline = Some(Box::pin(tokio::time::sleep(self.batcher_config.batch_timeout)));
                                } else {
                                    tracing::debug!(
                                        block_number,
                                        last_persisted_block = self.startup_config.last_persisted_block,
                                        "received block with number lower than `last_persisted_block`. Not enabling the deadline seal criteria yet."
                                    )
                                }
                            }
                        }
                        None => {
                            anyhow::bail!("Batcher's block receiver channel closed unexpectedly");
                        }
                    }
                }
            }
        }
        BATCHER_METRICS
            .blocks_per_batch
            .observe(blocks.len() as u64);
        accumulator.report_accumulated_resources_to_metrics();

        let protocol_version = &blocks.first().as_ref().unwrap().1.protocol_version;

        /* ---------- seal the batch ---------- */
        let batch_envelope = batch_builder::seal_batch(
            &blocks,
            prev_batch_info.clone(),
            batch_number,
            self.chain_id,
            self.chain_address,
            // we need to adapt pubdata mode depending on protocol version, to ensure automatic DA mode change during v30 upgrade
            self.pubdata_mode
                .adapt_for_protocol_version(protocol_version),
            self.sl_chain_id,
        )?;
        Ok(batch_envelope)
    }

    async fn recreate_existing_batch(
        &mut self,
        block_receiver: &mut PeekableReceiver<(
            BlockOutput,
            ReplayRecord,
            ProverInput,
            BlockMerkleTreeData,
        )>,
        latency_tracker: &ComponentStateHandle<GenericComponentState>,
        prev_batch_info: &StoredBatchInfo,
        existing_batch: DiscoveredCommittedBatch,
    ) -> anyhow::Result<BatchForSigning<ProverInput>> {
        let batch_number = existing_batch.number();

        tracing::info!(
            batch_number,
            first_block = existing_batch.first_block_number(),
            last_block = existing_batch.last_block_number(),
            "Recreating existing batch"
        );

        let mut blocks: Vec<(BlockOutput, ReplayRecord, TreeBatchOutput, ProverInput)> = vec![];

        let expected_block_count = existing_batch.block_count();
        // Collect all blocks in this batch
        while blocks.len() < expected_block_count as usize {
            latency_tracker.enter_state(GenericComponentState::WaitingRecv);
            let (block_output, replay_record, prover_input, tree) = block_receiver
                .recv()
                .await
                .context("channel closed while recreating batch")?;
            latency_tracker.enter_state(GenericComponentState::Processing);

            let (root_hash, leaf_count) = tree.block_end.root_info()?;
            let tree_output = TreeBatchOutput {
                root_hash,
                leaf_count,
            };

            tracing::debug!(
                batch_number,
                block_number = replay_record.block_context.block_number,
                "Adding block to recreated batch"
            );

            blocks.push((block_output, replay_record, tree_output, prover_input));
        }
        let last_block_number = blocks.last().unwrap().0.header.number;
        assert_eq!(
            last_block_number,
            existing_batch.last_block_number(),
            "Block number mismatch in last block of a rebuilt batch"
        );

        // Rebuild the batch from blocks
        let rebuilt_batch = batch_builder::seal_batch(
            &blocks,
            prev_batch_info.clone(),
            batch_number,
            self.chain_id,
            self.chain_address,
            // Assume pubdata mode does not change
            self.pubdata_mode,
            self.sl_chain_id,
        )?;

        // Verify that the rebuilt batch matches the stored batch by comparing hashes
        if self.batcher_config.assert_rebuilt_batch_hashes {
            let rebuilt_stored_batch_info = rebuilt_batch
                .batch
                .batch_info
                .clone()
                .into_stored(&rebuilt_batch.batch.protocol_version);

            anyhow::ensure!(
                rebuilt_stored_batch_info.hash() == existing_batch.batch_info.hash(),
                "Rebuilt batch info does not match stored batch info for batch {}. \
                 Rebuilt info: {:?}, Stored info: {:?}",
                batch_number,
                rebuilt_stored_batch_info,
                existing_batch.batch_info
            );
        } else {
            tracing::warn!(
                batch_number,
                "Batch hash verification is disabled - skipping verification of rebuilt batch"
            );
        }

        Ok(rebuilt_batch)
    }
}
