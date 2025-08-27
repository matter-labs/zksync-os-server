use crate::block_replay_storage::BlockReplayStorage;
use crate::config::SequencerConfig;
use crate::execution::block_context_provider::BlockContextProvider;
use crate::execution::block_executor::execute_block;
use crate::execution::metrics::{EXECUTION_METRICS, SequencerState};
use crate::execution::utils::save_dump;
use crate::model::blocks::{BlockCommand, ProduceCommand};
use anyhow::Context;
use futures::StreamExt;
use futures::stream::BoxStream;
use std::time::Duration;
use tokio::sync::mpsc::Sender;
use zk_os_forward_system::run::BlockOutput;
use zksync_os_observability::ComponentStateReporter;
use zksync_os_storage::lazy::RepositoryManager;
use zksync_os_storage_api::{ReadStateHistory, ReplayRecord, WriteState};

pub mod block_context_provider;
pub mod block_executor;
pub(crate) mod metrics;
pub(crate) mod utils;
pub mod vm_wrapper;

#[allow(clippy::too_many_arguments)]
pub async fn run_sequencer_actor<R: ReadStateHistory + WriteState + Clone>(
    mut block_stream: BoxStream<'_, BlockCommand>,

    prover_input_generator_sink: Sender<(BlockOutput, ReplayRecord)>,
    tree_sink: Sender<BlockOutput>,

    mut command_block_context_provider: BlockContextProvider<R>,
    state: R,
    wal: BlockReplayStorage,
    repositories: RepositoryManager,
    sequencer_config: SequencerConfig,
) -> anyhow::Result<()> {
    let latency_tracker =
        ComponentStateReporter::global().handle_for("sequencer", SequencerState::WaitingForCommand);
    loop {
        latency_tracker.enter_state(SequencerState::WaitingForCommand);

        let Some(cmd) = block_stream.next().await else {
            anyhow::bail!("inbound channel closed");
        };
        let block_number = cmd.block_number();

        tracing::info!(
            block_number,
            cmd = cmd.to_string(),
            "starting command. Turning into PreparedCommand.."
        );
        latency_tracker.enter_state(SequencerState::BlockContextTxs);

        let prepared_command = command_block_context_provider.prepare_command(cmd).await?;

        tracing::debug!(
            block_number,
            starting_l1_priority_id = prepared_command.starting_l1_priority_id,
            "Prepared command. Executing..",
        );

        let (block_output, replay_record, purged_txs) =
            execute_block(prepared_command, state.clone(), &latency_tracker)
                .await
                .map_err(|dump| {
                    let error = anyhow::anyhow!("{}", dump.error);
                    tracing::info!("Saving dump..");
                    if let Err(err) = save_dump(sequencer_config.block_dump_path.clone(), dump) {
                        tracing::error!("Failed to write dump: {err}");
                    }
                    error
                })
                .context("execute_block")?;

        tracing::debug!(block_number, "Executed. Adding to state...",);
        latency_tracker.enter_state(SequencerState::AddingToState);

        state.add_block_result(
            block_number,
            block_output.storage_writes.clone(),
            block_output
                .published_preimages
                .iter()
                .map(|(k, v, _)| (*k, v)),
        )?;

        tracing::debug!(block_number, "Added to state. Adding to repos...");
        latency_tracker.enter_state(SequencerState::AddingToRepos);

        // todo: do not call if api is not enabled.
        repositories
            .populate_in_memory_blocking(block_output.clone(), replay_record.transactions.clone())
            .await;

        tracing::debug!(block_number, "Added to repos. Updating mempools...",);
        latency_tracker.enter_state(SequencerState::UpdatingMempool);

        // TODO: would updating mempool in parallel with state make sense?
        command_block_context_provider.on_canonical_state_change(&block_output, &replay_record);
        let purged_txs_hashes = purged_txs.into_iter().map(|(hash, _)| hash).collect();
        command_block_context_provider.remove_txs(purged_txs_hashes);

        tracing::debug!(block_number, "Reported to mempools. Adding to wal...");
        latency_tracker.enter_state(SequencerState::AddingToWal);

        wal.append_replay(replay_record.clone());

        tracing::debug!(block_number, "Added to wal. Sending to batcher...");
        latency_tracker.enter_state(SequencerState::SendingToBatcher);

        prover_input_generator_sink
            .send((block_output.clone(), replay_record))
            .await?;

        tracing::debug!(block_number, "Sent to batcher. Sending to tree...");
        latency_tracker.enter_state(SequencerState::SendingToTree);

        tree_sink.send(block_output).await?;

        EXECUTION_METRICS.block_number[&"execute"].set(block_number);

        tracing::debug!(block_number, "Block fully processed");
    }
}

pub fn command_source(
    block_replay_wal: &BlockReplayStorage,
    block_to_start: u64,
    block_time: Duration,
    max_transactions_in_block: usize,
) -> BoxStream<BlockCommand> {
    let last_block_in_wal = block_replay_wal.latest_block().unwrap_or(0);
    tracing::info!(last_block_in_wal, "Last block in WAL: {last_block_in_wal}");
    tracing::info!(block_to_start, "block_to_start: {block_to_start}");

    // Stream of replay commands from WAL
    let replay_wal_stream: BoxStream<BlockCommand> =
        Box::pin(block_replay_wal.replay_commands_from(block_to_start));

    // Combined source: run WAL replay first, then produce blocks from mempool
    let produce_stream: BoxStream<BlockCommand> =
        futures::stream::unfold(last_block_in_wal + 1, move |block_number| async move {
            Some((
                BlockCommand::Produce(ProduceCommand {
                    block_number,
                    block_time,
                    max_transactions_in_block,
                }),
                block_number + 1,
            ))
        })
        .boxed();
    let stream = replay_wal_stream.chain(produce_stream);
    stream.boxed()
}
