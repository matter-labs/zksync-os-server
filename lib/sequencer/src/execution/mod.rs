use crate::config::SequencerConfig;
use crate::execution::block_context_provider::BlockContextProvider;
use crate::execution::block_executor::execute_block;
use crate::execution::metrics::{EXECUTION_METRICS, SequencerState};
use crate::execution::utils::save_dump;
use crate::model::blocks::BlockCommand;
use alloy::consensus::Sealed;
use anyhow::Context;
use async_trait::async_trait;
use tokio::sync::{mpsc::Sender, watch};
use tokio::time::Instant;
use zksync_os_interface::types::BlockOutput;
use zksync_os_mempool::subpools::l2::L2Subpool;
use zksync_os_observability::{ComponentStateHandle, ComponentStateReporter};
use zksync_os_pipeline::{PeekableReceiver, PipelineComponent};
use zksync_os_storage_api::{
    ReadStateHistory, ReplayRecord, WriteReplay, WriteRepository, WriteState,
};
use zksync_os_types::{NotAcceptingReason, TransactionAcceptanceState};

pub use fee_provider::{FeeConfig, FeeParams, FeeProvider};

pub mod block_context_provider;
pub mod block_executor;
mod fee_provider;
pub(crate) mod metrics;
pub(crate) mod utils;
pub mod vm_wrapper;

/// Sequencer pipeline component
/// Contains all the dependencies needed to run the sequencer
pub struct Sequencer<Subpool, State, Replay, Repo>
where
    Subpool: L2Subpool + Send + 'static,
    State: ReadStateHistory + WriteState + Clone + Send + 'static,
    Replay: WriteReplay + Send + 'static,
    Repo: WriteRepository + Send + 'static,
{
    pub block_context_provider: BlockContextProvider<Subpool>,
    pub state: State,
    pub replay: Replay,
    pub repositories: Repo,
    pub config: SequencerConfig,
    /// Controls transaction acceptance state.
    /// When max_blocks_to_produce limit is reached, sequencer sends NotAccepting to stop RPC from accepting new txs.
    pub tx_acceptance_state_sender: watch::Sender<TransactionAcceptanceState>,
}

#[async_trait]
impl<Subpool, State, Replay, Repo> PipelineComponent for Sequencer<Subpool, State, Replay, Repo>
where
    Subpool: L2Subpool + Send + 'static,
    State: ReadStateHistory + WriteState + Clone + Send + 'static,
    Replay: WriteReplay + Send + 'static,
    Repo: WriteRepository + Send + 'static,
{
    type Input = BlockCommand;
    type Output = (BlockOutput, ReplayRecord);

    const NAME: &'static str = "sequencer";
    const OUTPUT_BUFFER_SIZE: usize = 5;

    async fn run(
        mut self,
        mut input: PeekableReceiver<Self::Input>, // PeekableReceiver<BlockCommand>
        output: Sender<Self::Output>,             // Sender<BlockOutput>
    ) -> anyhow::Result<()> {
        let latency_tracker = ComponentStateReporter::global()
            .handle_for("sequencer", SequencerState::WaitingForCommand);

        // Track how many Produce commands we've processed (for `sequencer_max_blocks_to_produce` config)
        let mut produced_blocks_count = 0u64;

        // Only used for metrics/logs
        let mut last_processed_block_at: Option<Instant> = None;

        loop {
            latency_tracker.enter_state(SequencerState::WaitingForCommand);

            let Some(cmd) = input.recv().await else {
                anyhow::bail!("inbound channel closed");
            };
            let block_number = cmd.block_number();

            // For Produce commands: check limit (will await indefinitely if limit reached) and increment counter
            if matches!(cmd, BlockCommand::Produce(_))
                && let Some(limit) = self.config.max_blocks_to_produce
            {
                check_block_production_limit(
                    limit,
                    produced_blocks_count,
                    &self.tx_acceptance_state_sender,
                    &latency_tracker,
                )
                .await;
                produced_blocks_count += 1;
            }
            let override_allowed = match &cmd {
                BlockCommand::Rebuild(_) => true,
                BlockCommand::Replay(_) if self.config.node_role.is_external() => true,
                _ => false,
            };

            tracing::info!(
                block_number,
                cmd = cmd.to_string(),
                "starting command. Turning into PreparedCommand.."
            );
            latency_tracker.enter_state(SequencerState::BlockContextTxs);

            let prepared_command = self.block_context_provider.prepare_command(cmd).await?;

            tracing::debug!(
                block_number,
                starting_l1_priority_id = prepared_command.starting_l1_priority_id,
                "Prepared command. Executing..",
            );

            let (block_output, replay_record, purged_txs) =
                execute_block(prepared_command, self.state.clone(), &latency_tracker)
                    .await
                    .map_err(|dump| {
                        let error = anyhow::anyhow!("{}", dump.error);
                        tracing::info!("Saving dump..");
                        if let Err(err) = save_dump(self.config.block_dump_path.clone(), dump) {
                            tracing::error!(?err, "Failed to write block dump");
                        }
                        error
                    })
                    .context("execute_block")?;

            let time_since_last_block = last_processed_block_at
                .map(|last_processed_block_at| last_processed_block_at.elapsed());
            if let Some(time_since_last_block) = time_since_last_block {
                EXECUTION_METRICS
                    .time_since_last_block
                    .observe(time_since_last_block);
            }
            last_processed_block_at = Some(Instant::now());

            tracing::debug!(block_number, "Executed. Adding to block replay storage...");
            latency_tracker.enter_state(SequencerState::AddingToReplayStorage);

            self.replay.write(
                Sealed::new_unchecked(replay_record.clone(), block_output.header.hash()),
                override_allowed,
            );

            tracing::debug!(block_number, "Added to replay storage. Adding to state...");
            latency_tracker.enter_state(SequencerState::AddingToState);

            // Although, the plan is to always allow overrides for each storage except for replay,
            // for FullDiffs state backend it requires iterating over each storage write which is costly.
            // Therefore, we pass the override_allowed flag here. If it's set to true then override happens, otherwise,
            // changes are validated against existing storage.
            self.state.add_block_result(
                block_number,
                block_output.storage_writes.clone(),
                block_output
                    .published_preimages
                    .iter()
                    .map(|(k, v)| (*k, v)),
                override_allowed,
            )?;

            tracing::debug!(block_number, "Added to state. Adding to repos...");
            latency_tracker.enter_state(SequencerState::AddingToRepos);

            // todo: do not call if api is not enabled.
            self.repositories
                .populate(block_output.clone(), replay_record.transactions.clone())
                .await?;

            tracing::debug!(block_number, "Added to repos. Updating mempools...",);
            latency_tracker.enter_state(SequencerState::UpdatingMempool);

            // TODO: would updating mempool in parallel with state make sense?
            self.block_context_provider
                .on_canonical_state_change(&block_output, &replay_record)
                .await;
            let purged_txs_hashes = purged_txs.into_iter().map(|(hash, _)| hash).collect();
            self.block_context_provider
                .remove_transactions(purged_txs_hashes);

            tracing::debug!(
                block_number,
                time_since_last_block = ?time_since_last_block,
                "Block processed in sequencer! Sending downstream..."
            );
            EXECUTION_METRICS.block_number.set(block_number);
            EXECUTION_METRICS
                .last_execution_version
                .set(replay_record.block_context.execution_version as u64);

            latency_tracker.enter_state(SequencerState::WaitingSend);
            if output
                .send((block_output.clone(), replay_record.clone()))
                .await
                .is_err()
            {
                anyhow::bail!("Outbound channel closed");
            }

            tracing::debug!(block_number, "Block fully processed");
        }
    }
}

/// Checks if block production limit has been reached.
/// If limit is reached, signals to stop accepting transactions and awaits indefinitely (never returns).
/// Should only be called for Produce commands.
async fn check_block_production_limit(
    limit: u64,
    already_produced_blocks_count: u64,
    tx_acceptance_state_sender: &watch::Sender<TransactionAcceptanceState>,
    latency_tracker: &ComponentStateHandle<SequencerState>,
) {
    if already_produced_blocks_count >= limit {
        tracing::warn!(
            already_produced_blocks_count,
            limit,
            "Reached max_blocks_to_produce limit, stopping transaction acceptance"
        );

        // Signal to RPC that we're no longer accepting transactions
        let _ = tx_acceptance_state_sender.send(TransactionAcceptanceState::NotAccepting(
            NotAcceptingReason::BlockProductionDisabled,
        ));

        latency_tracker.enter_state(SequencerState::ConfiguredBlockLimitReached);
        std::future::pending::<()>().await;
    }
}
