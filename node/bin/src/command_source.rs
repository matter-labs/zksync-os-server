use async_trait::async_trait;
use std::collections::HashSet;
use tokio::sync::mpsc;
use zksync_os_pipeline::{PeekableReceiver, PipelineComponent};
use zksync_os_raft::{ConsensusRole, LeadershipSignal};
use zksync_os_sequencer::execution::block_context_provider::millis_since_epoch;
use zksync_os_sequencer::model::blocks::{BlockCommand, ProduceCommand, RebuildCommand};
use zksync_os_storage_api::{ReadReplay, ReadReplayExt, ReplayRecord};

/// Command source for consensus-enabled main node.
/// Replays local WAL starting from `starting_block` and then produces new blocks when leader.
#[derive(Debug)]
pub struct ConsensusNodeCommandSource<Replay> {
    /// Local block replays (aka `WAL`).
    pub block_replay_storage: Replay,
    /// Block number to start replaying from.
    pub starting_block: u64,
    /// If set, the node will start with proposing block rebuilds for already sealed blocks
    /// This is essentially a block rollback.
    pub rebuild_options: Option<RebuildOptions>,
    /// Inbound channel of canonized blocks. Populated by `BlockCanonizer` with blocks that are canonized
    pub replays_to_execute: mpsc::Receiver<ReplayRecord>,
    /// Current leadership status from consensus.
    pub leadership: LeadershipSignal,
}

#[derive(Debug)]
pub struct RebuildOptions {
    pub rebuild_from_block: u64,
    pub blocks_to_empty: HashSet<u64>,
}

/// External node command source.
#[derive(Debug)]
pub struct ExternalNodeCommandSource {
    pub up_to_block: Option<u64>,
    pub replays_for_sequencer: mpsc::Receiver<ReplayRecord>,
}

#[async_trait]
impl<Replay: ReadReplay> PipelineComponent for ConsensusNodeCommandSource<Replay> {
    type Input = ();
    type Output = BlockCommand;

    const NAME: &'static str = "consensus_node_command_source";
    const OUTPUT_BUFFER_SIZE: usize = 1;

    async fn run(
        mut self,
        _input: PeekableReceiver<()>,
        output: mpsc::Sender<BlockCommand>,
    ) -> anyhow::Result<()> {
        let last_block_in_wal = self.block_replay_storage.latest_record();

        let replay_until = if let Some(rebuild_options) = &self.rebuild_options {
            assert!(
                rebuild_options.rebuild_from_block >= self.starting_block,
                "rebuild_from_block must be >= starting_block, got {} < {}",
                rebuild_options.rebuild_from_block,
                self.starting_block
            );
            assert!(
                rebuild_options.rebuild_from_block <= last_block_in_wal,
                "rebuild_from_block must be <= last_block_in_wal, got {} > {}",
                rebuild_options.rebuild_from_block,
                last_block_in_wal
            );
            rebuild_options.rebuild_from_block - 1
        } else {
            last_block_in_wal
        };

        tracing::info!(
            "Replaying WAL blocks from {} until {}.",
            self.starting_block,
            replay_until
        );

        self.block_replay_storage
            .forward_range_with(
                self.starting_block,
                replay_until,
                output.clone(),
                |record| BlockCommand::Replay(Box::new(record)),
            )
            .await?;

        if let Some(rebuild_options) = &self.rebuild_options {
            self.send_block_rebuilds(rebuild_options, last_block_in_wal, &output)
                .await?;
        }

        tracing::info!("All WAL blocks replayed. Starting main loop.");

        self.run_loop(output).await
    }
}

impl<Replay: ReadReplay> ConsensusNodeCommandSource<Replay> {
    /// This method kicks in after all local canonized Replayed Records (WAL) are replayed.
    /// Produces `Produce` commands only when the node is the leader.
    async fn run_loop(mut self, output: mpsc::Sender<BlockCommand>) -> anyhow::Result<()> {
        let mut leadership = self.leadership.clone();
        let mut role = leadership.current_role();
        tracing::info!(?role, "Consensus role initialized");

        loop {
            tokio::select! {
                res = leadership.wait_for_change() => {
                    if res.is_err() {
                        anyhow::bail!("leader watch channel closed");
                    }
                    let new_role = leadership.current_role();
                    if new_role != role {
                        tracing::info!(?role, ?new_role, "Consensus role changed");
                        role = new_role;
                    }
                }
                maybe_record = self.replays_to_execute.recv() => {
                    let Some(record) = maybe_record else {
                        tracing::info!("inbound channel closed");
                        return Ok(());
                    };
                    tracing::info!(
                        block_number = record.block_context.block_number,
                        role = ?role,
                        "Received canonized block from consensus",
                    );
                    if output
                        .send(BlockCommand::Replay(Box::new(record)))
                        .await
                        .is_err()
                    {
                        tracing::warn!("Command output channel closed, stopping source");
                        break;
                    }
                }
                send_res = output.send(BlockCommand::Produce(ProduceCommand)), if role == ConsensusRole::Leader => {
                    if send_res.is_err() {
                        tracing::warn!("Command output channel closed, stopping source");
                        break;
                    }
                }
            }
        }

        Ok(())
    }

    async fn send_block_rebuilds(
        &self,
        rebuild_options: &RebuildOptions,
        last_block_in_wal: u64,
        output: &mpsc::Sender<BlockCommand>,
    ) -> anyhow::Result<()> {
        tracing::warn!(
            "Starting block rebuilds! {rebuild_options:?}, last_block_in_wal: {last_block_in_wal}"
        );
        for block_number in rebuild_options.rebuild_from_block..=last_block_in_wal {
            let replay_record = self
                .block_replay_storage
                .get_replay_record(block_number)
                .expect("Replay record must exist for rebuild");
            let make_empty = rebuild_options.blocks_to_empty.contains(&block_number);
            tracing::warn!(
                "Processing block rebuild {block_number} with original block_output_hash {:?}, \
                 timestamp {} ({} seconds ago), make_empty: {make_empty}.",
                replay_record.block_output_hash,
                replay_record.block_context.timestamp,
                (millis_since_epoch() / 1000) as u64 - replay_record.block_context.timestamp
            );
            let command = BlockCommand::Rebuild(Box::new(RebuildCommand {
                replay_record,
                make_empty,
            }));
            if output.send(command).await.is_err() {
                tracing::warn!("Command output channel closed, stopping source");
                break;
            }
        }
        Ok(())
    }
}

#[async_trait]
impl PipelineComponent for ExternalNodeCommandSource {
    type Input = ();
    type Output = BlockCommand;

    const NAME: &'static str = "external_node_command_source";
    const OUTPUT_BUFFER_SIZE: usize = 5;

    async fn run(
        mut self,
        _input: PeekableReceiver<()>,
        output: mpsc::Sender<BlockCommand>,
    ) -> anyhow::Result<()> {
        while let Some(record) = self.replays_for_sequencer.recv().await {
            let block_number = record.block_context.block_number;
            let command = BlockCommand::Replay(Box::new(record));
            tracing::debug!(?command, "Received block command from main node");

            if let Some(up_to_block) = self.up_to_block
                && block_number > up_to_block
            {
                tracing::info!(
                    up_to_block,
                    "Reached up_to_block, halting external command source"
                );
                futures::future::pending::<()>().await;
            }

            if output.send(command).await.is_err() {
                tracing::warn!("Command output channel closed, stopping source");
                break;
            }
        }

        Ok(())
    }
}
