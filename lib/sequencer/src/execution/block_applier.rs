use crate::config::SequencerConfig;
use crate::execution::metrics::BlockApplierState;
use crate::model::blocks::{AppliedBlock, BlockCommandType, BlockOutputWithReads, BlockPayload};
use alloy::consensus::Sealed;
use alloy::primitives::BlockNumber;
use anyhow::Context as _;
use async_trait::async_trait;
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
use zksync_os_observability::ComponentStateReporter;
use zksync_os_pipeline::{PeekableReceiver, PipelineComponent, SendAndRecordExt};
use zksync_os_storage_api::{
    RepositoryResult, ReplayRecord, WriteReplay, WriteRepository, WriteState,
};

/// Persists blocks in various local storages.
/// Used to be part of the Sequencer - was split into `BlockExecutor` and `BlockApplier`.
pub struct BlockApplier<State, Replay, Repo>
where
    State: WriteState + Clone + Send + 'static,
    Replay: WriteReplay + Send + 'static,
    Repo: WriteRepository + Clone + Send + 'static,
{
    pub state: State,
    pub replay: Replay,
    pub repositories: Repo,
    pub config: SequencerConfig,
    pub applied_block_number_sender: watch::Sender<Option<BlockNumber>>,
}

#[async_trait]
impl<State, Replay, Repo> PipelineComponent for BlockApplier<State, Replay, Repo>
where
    State: WriteState + Clone + Send + 'static,
    Replay: WriteReplay + Send + 'static,
    Repo: WriteRepository + Clone + Send + 'static,
{
    type Input = BlockPayload;
    type Output = AppliedBlock;

    const COMPONENT_ID: zksync_os_pipeline::ComponentId =
        zksync_os_pipeline::ComponentId::BlockApplier;

    async fn run(
        mut self,
        mut input: PeekableReceiver<Self::Input>,
        output: mpsc::Sender<Self::Output>,
        state_reporter: ComponentStateReporter,
    ) -> anyhow::Result<()> {
        // Sliding window: the cheap, order-sensitive work (replay write + state diff) stays
        // sequential as blocks stream in, but the expensive per-block repository `populate` (~25ms,
        // the bottleneck) is spawned without awaiting, so up to `parallel_blocks` populates run
        // concurrently across the round's disjoint blocks. The oldest is awaited + forwarded once the
        // window is full, preserving downstream block order. `parallel_blocks == 1` (production)
        // awaits each populate immediately -> unchanged one-at-a-time behaviour.
        let window = self.config.parallel_blocks.max(1);
        // (block_number, output, record, populate join handle), oldest at the front.
        let mut in_flight: std::collections::VecDeque<(
            u64,
            BlockOutputWithReads,
            ReplayRecord,
            Duration,
            Duration,
            tokio::task::JoinHandle<RepositoryResult<()>>,
        )> = std::collections::VecDeque::with_capacity(window);
        loop {
            state_reporter.enter_state(BlockApplierState::Idle);
            let Some(BlockPayload {
                output: block_output_with_reads,
                record: executed_replay,
                command_type: cmd_type,
                failed_transactions,
            }) = input.recv_and_record_picked(&state_reporter).await
            else {
                // Channel closed: drain the in-flight window in order, then stop.
                while let Some((bn, out, rec, _, _, handle)) = in_flight.pop_front() {
                    handle.await.context("populate task panicked")??;
                    self.applied_block_number_sender.send_replace(Some(bn));
                    output.send_and_record(
                        AppliedBlock {
                            output: out,
                            record: rec,
                        },
                        &state_reporter,
                    )?;
                }
                tracing::info!("inbound channel closed");
                return Ok(());
            };

            let block_output = block_output_with_reads.as_ref();
            let block_number = executed_replay.block_context.block_number;
            let block_hash = block_output.header.hash();
            let override_allowed = match cmd_type {
                BlockCommandType::Rebuild => true,
                _ if self.config.node_role.is_external() => true,
                _ => false,
            };

            state_reporter.enter_state(BlockApplierState::AddingToStorage);
            // Bench probe: skip the replay-record WAL write in the parallel-blocks bench (no restart /
            // replay / proving needed there). The serial path (production, parallel_blocks == 1) is
            // unaffected.
            if self.config.parallel_blocks <= 1 {
                if let Err(err) = self
                    .replay
                    .write(
                        Sealed::new_unchecked(executed_replay.clone(), block_hash),
                        override_allowed,
                    )
                    .await
                {
                    tracing::info!("Failed to write replay record: {err}, shutting down");
                    return Ok(());
                }
            }

            // Sequential + in order: storage-diff contiguity requires ascending block numbers.
            let t_add_state = Instant::now();
            self.state.add_block_result(
                block_number,
                block_output.storage_writes.clone(),
                block_output.published_preimages.iter().map(|(k, v)| (*k, v)),
                override_allowed,
            )?;
            let add_state_elapsed = t_add_state.elapsed();

            // Spawn the expensive repository population (independent across disjoint blocks); do not
            // await it yet, so it overlaps with the next blocks' ingestion.
            let t_populate_spawn = Instant::now();
            let repositories = self.repositories.clone();
            let block_output_owned = block_output.clone();
            let transactions = executed_replay.transactions.clone();
            let handle = tokio::spawn(async move {
                repositories
                    .populate(block_output_owned, transactions, failed_transactions)
                    .await
            });
            let populate_spawn_elapsed = t_populate_spawn.elapsed();
            in_flight.push_back((
                block_number,
                block_output_with_reads,
                executed_replay,
                add_state_elapsed,
                populate_spawn_elapsed,
                handle,
            ));

            // Once `window` populates are in flight, await + forward the oldest (keeps order).
            if in_flight.len() >= window {
                state_reporter.enter_state(BlockApplierState::PopulatingRepos);
                let (bn, out, rec, add_state_elapsed, populate_spawn_elapsed, handle) =
                    in_flight.pop_front().unwrap();
                let t_populate_wait = Instant::now();
                handle.await.context("populate task panicked")??;
                let populate_wait_elapsed = t_populate_wait.elapsed();
                self.applied_block_number_sender.send_replace(Some(bn));
                let t_output_send = Instant::now();
                output.send_and_record(
                    AppliedBlock {
                        output: out,
                        record: rec,
                    },
                    &state_reporter,
                )?;
                let output_send_elapsed = t_output_send.elapsed();
                if self.config.parallel_blocks > 1 {
                    tracing::info!(
                        block_number = bn,
                        ?add_state_elapsed,
                        ?populate_spawn_elapsed,
                        ?populate_wait_elapsed,
                        ?output_send_elapsed,
                        "block_applier block done"
                    );
                }
            }
        }
    }
}
