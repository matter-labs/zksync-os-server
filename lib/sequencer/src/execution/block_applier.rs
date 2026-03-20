use crate::config::SequencerConfig;
use crate::model::blocks::BlockCommandType;
use alloy::consensus::Sealed;
use async_trait::async_trait;
use tokio::sync::mpsc;
use zksync_os_interface::types::BlockOutput;
use zksync_os_observability::{ComponentHealthReporter, GenericComponentState};
use zksync_os_pipeline::{PeekableReceiver, PipelineComponent};
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
    pub health_reporter: ComponentHealthReporter,
}

#[async_trait]
impl<State, Replay, Repo> PipelineComponent for BlockApplier<State, Replay, Repo>
where
    State: WriteState + Clone + Send + 'static,
    Replay: WriteReplay + Send + 'static,
    Repo: WriteRepository + Send + 'static,
{
    type Input = (BlockOutput, ReplayRecord, BlockCommandType);
    type Output = (BlockOutput, ReplayRecord);

    const NAME: &'static str = "block_applier";
    const OUTPUT_BUFFER_SIZE: usize = 5;

    async fn run(
        mut self,
        mut input: PeekableReceiver<Self::Input>,
        output: mpsc::Sender<Self::Output>,
    ) -> anyhow::Result<()> {
        loop {
            self.health_reporter
                .enter_state(GenericComponentState::WaitingRecv);
            let Some((block_output, executed_replay, cmd_type)) = input.recv().await else {
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
                .enter_state(GenericComponentState::Processing);
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

            self.repositories
                .populate(block_output.clone(), executed_replay.transactions.clone())
                .await?;

            self.health_reporter
                .enter_state(GenericComponentState::WaitingSend);
            if output.send((block_output, executed_replay)).await.is_err() {
                tracing::info!("outbound channel closed");
                return Ok(());
            }
            self.health_reporter.record_processed(block_number);
        }
    }
}
