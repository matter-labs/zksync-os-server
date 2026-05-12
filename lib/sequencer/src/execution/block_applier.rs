use crate::config::SequencerConfig;
use crate::execution::metrics::BlockApplierState;
use crate::execution::metrics::REPLAY_ARCHIVE_METRICS;
use crate::model::blocks::{AppliedBlock, BlockCommandType, BlockPayload};
use alloy::consensus::Sealed;
use alloy::primitives::BlockHash;
use async_trait::async_trait;
use std::time::Instant;
use tokio::sync::{mpsc, watch};
use zksync_os_observability::ComponentStateReporter;
use zksync_os_pipeline::{PeekableReceiver, PipelineComponent, SendAndRecordExt};
use zksync_os_storage_api::{ReplayRecord, WriteReplay, WriteRepository, WriteState};

/// Persists blocks in various local storages.
/// Used to be part of the Sequencer - was split into `BlockExecutor` and `BlockApplier`.
pub struct BlockApplier<State, Replay, Repo>
where
    State: WriteState + Clone + Send + 'static,
    Replay: WriteReplay + Send + 'static,
    Repo: WriteRepository + Send + 'static,
{
    pub state: State,
    pub replay: Replay,
    pub repositories: Repo,
    pub config: SequencerConfig,
    pub applied_block_number_sender: watch::Sender<u64>,
    pub replay_archive_sender: Option<mpsc::Sender<(BlockHash, ReplayRecord)>>,
}

#[async_trait]
impl<State, Replay, Repo> PipelineComponent for BlockApplier<State, Replay, Repo>
where
    State: WriteState + Clone + Send + 'static,
    Replay: WriteReplay + Send + 'static,
    Repo: WriteRepository + Send + 'static,
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
        loop {
            state_reporter.enter_state(BlockApplierState::Idle);
            let Some(BlockPayload {
                output: block_output,
                record: executed_replay,
                command_type: cmd_type,
            }) = input.recv_and_record_picked(&state_reporter).await
            else {
                tracing::info!("inbound channel closed");
                return Ok(());
            };

            let block_number = executed_replay.block_context.block_number;
            let block_hash = block_output.header.hash();
            let override_allowed = match cmd_type {
                BlockCommandType::Rebuild => true,
                _ if self.config.node_role.is_external() => true,
                _ => false,
            };

            state_reporter.enter_state(BlockApplierState::AddingToStorage);
            tracing::info!(block_number, "Persisting block {block_number}");
            self.replay.write(
                Sealed::new_unchecked(executed_replay.clone(), block_hash),
                override_allowed,
            );

            if let Some(replay_archive_sender) = &self.replay_archive_sender {
                REPLAY_ARCHIVE_METRICS
                    .queue_depth
                    .set(replay_archive_queue_depth(replay_archive_sender));
                let started_at = Instant::now();
                let send_result = replay_archive_sender
                    .send((block_hash, executed_replay.clone()))
                    .await;
                REPLAY_ARCHIVE_METRICS
                    .enqueue_latency
                    .observe(started_at.elapsed());
                REPLAY_ARCHIVE_METRICS
                    .queue_depth
                    .set(replay_archive_queue_depth(replay_archive_sender));
                send_result.map_err(|_| anyhow::anyhow!("replay archive component stopped"))?;
            }

            self.state.add_block_result(
                block_number,
                block_output.storage_writes.clone(),
                block_output
                    .published_preimages
                    .iter()
                    .map(|(k, v)| (*k, v)),
                override_allowed,
            )?;

            state_reporter.enter_state(BlockApplierState::PopulatingRepos);
            self.repositories
                .populate(block_output.clone(), executed_replay.transactions.clone())
                .await?;

            self.applied_block_number_sender.send_replace(block_number);

            output.send_and_record(
                AppliedBlock {
                    output: block_output,
                    record: executed_replay,
                },
                &state_reporter,
            )?;
        }
    }
}

fn replay_archive_queue_depth(sender: &mpsc::Sender<(BlockHash, ReplayRecord)>) -> usize {
    sender.max_capacity() - sender.capacity()
}
