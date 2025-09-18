use crate::config::SequencerConfig;
use crate::execution::block_context_provider::BlockContextProvider;
use crate::execution::block_executor::execute_block;
use crate::execution::metrics::{EXECUTION_METRICS, SequencerState};
use crate::execution::utils::save_dump;
use crate::model::blocks::BlockCommand;
use anyhow::Context;
use futures::StreamExt;
use futures::stream::BoxStream;
use tokio::sync::mpsc::Sender;
use tokio::task::JoinHandle;
use zksync_os_interface::types::BlockOutput;
use zksync_os_mempool::L2TransactionPool;
use zksync_os_observability::ComponentStateReporter;
use zksync_os_storage_api::{
    ReadStateHistory, ReplayRecord, WriteReplay, WriteRepository, WriteState,
};

pub mod block_context_provider;
pub mod block_executor;
pub(crate) mod metrics;
pub(crate) mod utils;
pub mod vm_wrapper;

#[allow(clippy::too_many_arguments)]
pub async fn run_sequencer_actor<
    State: ReadStateHistory + WriteState + Clone,
    Mempool: L2TransactionPool,
>(
    mut block_stream: BoxStream<'_, BlockCommand>,

    prover_input_generator_sink: Sender<(BlockOutput, ReplayRecord)>,
    tree_sink: Sender<BlockOutput>,

    mut command_block_context_provider: BlockContextProvider<Mempool>,
    state: State,
    wal: impl WriteReplay,
    repositories: impl WriteRepository,
    sequencer_config: SequencerConfig,
) -> anyhow::Result<()> {
    let latency_tracker =
        ComponentStateReporter::global().handle_for("sequencer", SequencerState::WaitingForCommand);
    let mut prev_rep: Option<JoinHandle<()>> = None;
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
                .map(|(k, v)| (*k, v)),
        )?;

        tracing::debug!(block_number, "Added to state. Adding to repos...");
        latency_tracker.enter_state(SequencerState::AddingToRepos);

        if let Some(e) = prev_rep {
            e.await.expect("ouch");
        }

        let repos = repositories.clone();            // Arc<dyn WriteRepository + Send + Sync + 'static>
        let b = block_output.clone();
        let rr = replay_record.transactions.clone();

        prev_rep = Some(tokio::spawn(async move {
            // repos is moved (owned) by this future, so borrows of &*repos are valid
            repos.populate(b, rr).await;
        }));

        tracing::debug!(block_number, "Added to repos. Updating mempools...",);
        latency_tracker.enter_state(SequencerState::UpdatingMempool);

        // TODO: would updating mempool in parallel with state make sense?
        command_block_context_provider.on_canonical_state_change(&block_output, &replay_record);
        let purged_txs_hashes = purged_txs.into_iter().map(|(hash, _)| hash).collect();
        command_block_context_provider.remove_txs(purged_txs_hashes);

        tracing::debug!(block_number, "Reported to mempools. Adding to wal...");
        latency_tracker.enter_state(SequencerState::AddingToWal);

        wal.append(replay_record.clone());

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
