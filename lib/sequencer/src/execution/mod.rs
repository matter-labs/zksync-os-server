use crate::config::SequencerConfig;
use crate::execution::block_context_provider::BlockContextProvider;
use crate::execution::block_executor::execute_block;
use crate::execution::metrics::{EXECUTION_METRICS, SequencerState};
use crate::execution::utils::save_dump;
use crate::model::blocks::BlockCommand;
use anyhow::Context;
use async_trait::async_trait;
use tokio::sync::{mpsc::Sender, watch};
use zksync_os_interface::types::BlockOutput;
use zksync_os_mempool::L2TransactionPool;
use zksync_os_observability::{ComponentStateHandle, ComponentStateReporter};
use zksync_os_pipeline::{PeekableReceiver, PipelineComponent};
use zksync_os_storage_api::{
    ReadStateHistory, ReplayRecord, WriteReplay, WriteRepository, WriteState,
};
use zksync_os_types::{NotAcceptingReason, TransactionAcceptanceState};

pub mod block_context_provider;
pub mod block_executor;
pub(crate) mod metrics;
pub(crate) mod utils;
pub mod vm_wrapper;

/// Sequencer pipeline component
/// Contains all the dependencies needed to run the sequencer
pub struct Sequencer<Mempool, State, Wal, Repo>
where
    Mempool: L2TransactionPool + Send + 'static,
    State: ReadStateHistory + WriteState + Clone + Send + 'static,
    Wal: WriteReplay + Send + 'static,
    Repo: WriteRepository + Send + 'static,
{
    pub block_context_provider: BlockContextProvider<Mempool>,
    pub state: State,
    pub wal: Wal,
    pub repositories: Repo,
    pub sequencer_config: SequencerConfig,
    /// Controls transaction acceptance state.
    /// When max_blocks_to_produce limit is reached, sequencer sends NotAccepting to stop RPC from accepting new txs.
    pub tx_acceptance_state_sender: watch::Sender<TransactionAcceptanceState>,
}

#[async_trait]
impl<Mempool, State, Wal, Repo> PipelineComponent for Sequencer<Mempool, State, Wal, Repo>
where
    Mempool: L2TransactionPool + Send + 'static,
    State: ReadStateHistory + WriteState + Clone + Send + 'static,
    Wal: WriteReplay + Send + 'static,
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

        loop {
            latency_tracker.enter_state(SequencerState::WaitingForCommand);

            let Some(cmd) = input.recv().await else {
                anyhow::bail!("inbound channel closed");
            };
            let block_number = cmd.block_number();

            // For Produce commands: check limit (will await indefinitely if limit reached) and increment counter
            if matches!(cmd, BlockCommand::Produce(_))
                && let Some(limit) = self.sequencer_config.max_blocks_to_produce
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
                        if let Err(err) =
                            save_dump(self.sequencer_config.block_dump_path.clone(), dump)
                        {
                            tracing::error!("Failed to write dump: {err}");
                        }
                        error
                    })
                    .context("execute_block")?;

            tracing::debug!(block_number, "Executed. Adding to state...",);
            latency_tracker.enter_state(SequencerState::AddingToState);

            self.state.add_block_result(
                block_number,
                block_output.storage_writes.clone(),
                block_output
                    .published_preimages
                    .iter()
                    .map(|(k, v)| (*k, v)),
            )?;

            tracing::debug!(block_number, "Added to state. Adding to repos...");
            latency_tracker.enter_state(SequencerState::AddingToRepos);

            // todo: do not call if api is not enabled.
            self.repositories
                .populate(block_output.clone(), replay_record.transactions.clone())
                .await;

            tracing::debug!(block_number, "Added to repos. Updating mempools...",);
            latency_tracker.enter_state(SequencerState::UpdatingMempool);

            // TODO: would updating mempool in parallel with state make sense?
            self.block_context_provider
                .on_canonical_state_change(&block_output, &replay_record);
            let purged_txs_hashes = purged_txs.into_iter().map(|(hash, _)| hash).collect();
            self.block_context_provider.remove_txs(purged_txs_hashes);

            tracing::debug!(block_number, "Reported to mempools. Adding to wal...");
            latency_tracker.enter_state(SequencerState::AddingToWal);

            self.wal.append(replay_record.clone());

            tracing::debug!(
                block_number,
                "Block processed in sequencer! Sending downstream..."
            );
            EXECUTION_METRICS.block_number[&"execute"].set(block_number);

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
