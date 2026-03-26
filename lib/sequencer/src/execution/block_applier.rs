use crate::config::SequencerConfig;
use crate::execution::metrics::BlockApplierState;
use crate::model::blocks::{AppliedBlock, BlockCommandType, BlockPayload};
use alloy::consensus::Sealed;
use async_trait::async_trait;
use tokio::sync::watch;
use zksync_os_observability::ComponentHealthReporter;
use zksync_os_pipeline::{PipelineComponent, TrackedUnboundedReceiver, TrackedUnboundedSender};
use zksync_os_storage_api::{WriteReplay, WriteRepository, WriteState};

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
    pub health_reporter: ComponentHealthReporter,
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

    const NAME: &'static str = "block_applier";

    async fn run(
        mut self,
        mut input: TrackedUnboundedReceiver<Self::Input>,
        output: TrackedUnboundedSender<Self::Output>,
    ) -> anyhow::Result<()> {
        loop {
            self.health_reporter.enter_state(BlockApplierState::Idle);
            // `recv_and_record` marks this block as processed at receive time —
            // before storage writes or repo population are complete.
            // This is intentional: the pipeline health monitor uses this for
            // backpressure signals, not durability guarantees.
            let Some(BlockPayload {
                output: block_output,
                record: executed_replay,
                command_type: cmd_type,
            }) = input.recv_and_record(&self.health_reporter).await
            else {
                tracing::info!("inbound channel closed");
                return Ok(());
            };

            let block_number = executed_replay.block_context.block_number;
            let override_allowed = match cmd_type {
                BlockCommandType::Rebuild => true,
                _ if self.config.node_role.is_external() => true,
                _ => false,
            };

            self.health_reporter
                .enter_state(BlockApplierState::AddingToStorage);
            tracing::info!(block_number, "Persisting block {block_number}");
            self.replay.write(
                Sealed::new_unchecked(executed_replay.clone(), block_output.header.hash()),
                override_allowed,
            );

            self.state.add_block_result(
                block_number,
                block_output.storage_writes.clone(),
                block_output
                    .published_preimages
                    .iter()
                    .map(|(k, v)| (*k, v)),
                override_allowed,
            )?;

            self.health_reporter
                .enter_state(BlockApplierState::PopulatingRepos);
            self.repositories
                .populate(block_output.clone(), executed_replay.transactions.clone())
                .await?;

            self.applied_block_number_sender.send_replace(block_number);

            if output
                .send(AppliedBlock {
                    output: block_output,
                    record: executed_replay,
                })
                .is_err()
            {
                tracing::info!("outbound channel closed");
                return Ok(());
            }
        }
    }
}
