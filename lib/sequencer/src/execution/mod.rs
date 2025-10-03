use crate::config::SequencerConfig;
use crate::execution::block_context_provider::BlockContextProvider;
use crate::execution::utils::save_dump;
use crate::model::blocks::BlockCommand;
use anyhow::Context;
use async_trait::async_trait;
use tokio::sync::mpsc::Sender;
use zksync_os_interface::types::BlockOutput;
use zksync_os_mempool::L2TransactionPool;
use zksync_os_observability::ComponentStateReporter;
use zksync_os_pipeline::{PeekableReceiver, PipelineComponent};
use zksync_os_storage_api::{
    ReadStateHistory, ReplayRecord, WriteReplay, WriteRepository, WriteState,
};
use crate::execution::block_executor::execute_block;
use crate::execution::metrics::{SequencerState, EXECUTION_METRICS};

pub mod block_context_provider;
pub mod block_executor;
pub(crate) mod metrics;
pub(crate) mod utils;
pub mod vm_wrapper;

/// Parameters for the Sequencer pipeline component
/// Contains all the dependencies needed to run the sequencer
pub struct SequencerParams<Mempool, State, Wal, Repo>
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
}

pub struct Sequencer<Mempool, State, Wal, Repo>
where
    Mempool: L2TransactionPool + Send + 'static,
    State: ReadStateHistory + WriteState + Clone + Send + 'static,
    Wal: WriteReplay + Send + 'static,
    Repo: WriteRepository + Send + 'static,
{
    _phantom: std::marker::PhantomData<(Mempool, State, Wal, Repo)>,
}

impl<Mempool, State, Wal, Repo> Default for Sequencer<Mempool, State, Wal, Repo>
where
    Mempool: L2TransactionPool + Send + 'static,
    State: ReadStateHistory + WriteState + Clone + Send + 'static,
    Wal: WriteReplay + Send + 'static,
    Repo: WriteRepository + Send + 'static,
{
    fn default() -> Self {
        Self {
            _phantom: std::marker::PhantomData,
        }
    }
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
    type Params = SequencerParams<Mempool, State, Wal, Repo>;

    const NAME: &'static str = "sequencer";
    const OUTPUT_BUFFER_SIZE: usize = 5;

    async fn run(
        params: Self::Params,
        mut input: PeekableReceiver<Self::Input>, // PeekableReceiver<BlockCommand>
        output: Sender<Self::Output>, // Sender<BlockOutput>
    ) -> anyhow::Result<()> {
        let mut command_block_context_provider = params.block_context_provider;
        let state = params.state;
        let wal = params.wal;
        let repositories = params.repositories;
        let sequencer_config = params.sequencer_config;

    let latency_tracker =
        ComponentStateReporter::global().handle_for("sequencer", SequencerState::WaitingForCommand);
    loop {
        latency_tracker.enter_state(SequencerState::WaitingForCommand);

        let Some(cmd) = input.recv().await else {
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

        // todo: do not call if api is not enabled.
        repositories
            .populate(block_output.clone(), replay_record.transactions.clone())
            .await;

        tracing::debug!(block_number, "Added to repos. Updating mempools...",);
        latency_tracker.enter_state(SequencerState::UpdatingMempool);

        // TODO: would updating mempool in parallel with state make sense?
        command_block_context_provider.on_canonical_state_change(&block_output, &replay_record);
        let purged_txs_hashes = purged_txs.into_iter().map(|(hash, _)| hash).collect();
        command_block_context_provider.remove_txs(purged_txs_hashes);

        tracing::debug!(block_number, "Reported to mempools. Adding to wal...");
        latency_tracker.enter_state(SequencerState::AddingToWal);

        wal.append(replay_record.clone());

        tracing::debug!(block_number, "Block processed in sequencer! Sending downstream...");
        EXECUTION_METRICS.block_number[&"execute"].set(block_number);

        latency_tracker.enter_state(SequencerState::WaitingSend);
        if output.send((block_output.clone(), replay_record.clone())).await.is_err() {
            anyhow::bail!("Outbound channel closed");
        }

        tracing::debug!(block_number, "Block fully processed");
    }
    }
}
