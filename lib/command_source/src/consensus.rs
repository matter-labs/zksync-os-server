use crate::{CommandWindow, DEFAULT_COMMAND_WINDOW_CAPACITY, ReplayCommandForwarder};
use async_trait::async_trait;
use std::collections::HashSet;
use tokio::sync::{mpsc, watch};
use zksync_os_observability::ComponentStateReporter;
use zksync_os_pipeline::{PeekableReceiver, PipelineComponent};
use zksync_os_raft::{ConsensusRole, LeadershipSignal};
use zksync_os_sequencer::execution::block_context_provider::millis_since_epoch;
use zksync_os_sequencer::model::blocks::{
    BlockCommand, BlockCommandType, CommandAck, ProduceCommand, RebuildCommand,
};
use zksync_os_storage_api::{ReadReplay, ReplayRecord};
use zksync_os_types::{NotAcceptingReason, TransactionAcceptanceState};

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
    pub replays_to_execute: mpsc::UnboundedReceiver<ReplayRecord>,
    /// Acknowledges that the previously emitted command crossed its downstream
    /// lifecycle boundary and the source may emit another one.
    pub command_acks: mpsc::Receiver<CommandAck>,
    /// Shared Replay forwarding and admission helper.
    pub replay_forwarder: ReplayCommandForwarder,
    /// Optional operational cap on newly produced blocks. Replays bypass this.
    pub max_blocks_to_produce: Option<u64>,
    /// Signals RPC admission when the configured production limit is reached.
    pub tx_acceptance_state_sender: watch::Sender<TransactionAcceptanceState>,
    /// Current leadership status from consensus.
    pub leadership: LeadershipSignal,
}

#[derive(Debug, Clone)]
pub struct RebuildOptions {
    pub from_block: u64,
    pub blocks_to_empty: HashSet<u64>,
    pub reset_timestamps: bool,
}

#[async_trait]
impl<Replay: ReadReplay> PipelineComponent for ConsensusNodeCommandSource<Replay> {
    type Input = ();
    type Output = BlockCommand;

    const COMPONENT_ID: zksync_os_pipeline::ComponentId =
        zksync_os_pipeline::ComponentId::ConsensusNodeCommandSource;

    async fn run(
        mut self,
        _input: PeekableReceiver<()>,
        output: mpsc::Sender<BlockCommand>,
        state_reporter: ComponentStateReporter,
    ) -> anyhow::Result<()> {
        let last_block_in_wal = self.block_replay_storage.latest_record();

        let replay_until = if let Some(rebuild_options) = &self.rebuild_options {
            assert!(
                rebuild_options.from_block >= self.starting_block,
                "rebuild_from_block must be >= starting_block, got {} < {}",
                rebuild_options.from_block,
                self.starting_block
            );
            assert!(
                rebuild_options.from_block <= last_block_in_wal,
                "rebuild_from_block must be <= last_block_in_wal, got {} > {}",
                rebuild_options.from_block,
                last_block_in_wal
            );
            rebuild_options.from_block - 1
        } else {
            last_block_in_wal
        };

        tracing::info!(
            "Replaying WAL blocks from {} until {}.",
            self.starting_block,
            replay_until
        );

        self.forward_wal_replays(self.starting_block, replay_until, &output)
            .await?;

        if let Some(rebuild_options) = self.rebuild_options.clone() {
            self.send_block_rebuilds(&rebuild_options, last_block_in_wal, &output)
                .await?;
        }

        tracing::info!("All WAL blocks replayed. Starting main loop.");

        // Seed watermark so block_diff_to_head starts at 0; leader mode never fires maybe_record.
        if let Some(ctx) = self.block_replay_storage.get_context(last_block_in_wal) {
            state_reporter.record_processed(last_block_in_wal, Some(ctx.timestamp), None);
        }

        self.run_loop(output, state_reporter).await
    }
}

impl<Replay: ReadReplay> ConsensusNodeCommandSource<Replay> {
    /// This method kicks in after all local canonized Replayed Records (WAL) are replayed.
    /// Produces `Produce` commands only when the node is the leader.
    async fn run_loop(
        mut self,
        output: mpsc::Sender<BlockCommand>,
        state_reporter: ComponentStateReporter,
    ) -> anyhow::Result<()> {
        let mut leadership = self.leadership.clone();
        let mut role = leadership.current_role();
        let mut command_window = CommandWindow::new(DEFAULT_COMMAND_WINDOW_CAPACITY);
        let mut produced_blocks_count = 0u64;
        let mut production_limit_reported = false;
        tracing::info!(?role, "Consensus role initialized");

        loop {
            let gate_open = self.replay_forwarder.is_open();
            let production_limit_reached = self
                .max_blocks_to_produce
                .is_some_and(|limit| produced_blocks_count >= limit);
            if production_limit_reached && !production_limit_reported {
                tracing::warn!(
                    produced_blocks_count,
                    limit = self.max_blocks_to_produce,
                    "Reached max_blocks_to_produce limit, stopping transaction acceptance"
                );
                let _ =
                    self.tx_acceptance_state_sender
                        .send(TransactionAcceptanceState::NotAccepting(vec![
                            NotAcceptingReason::BlockProductionDisabled,
                        ]));
                production_limit_reported = true;
            }

            let can_produce = role == ConsensusRole::Leader
                && command_window.can_send(BlockCommandType::Produce)
                && gate_open
                && !production_limit_reached;
            let can_replay = gate_open && command_window.can_send(BlockCommandType::Replay);

            // Priority is intentional: handle role changes and downstream acks first,
            // then canonized replays, and emit fresh Produce only as the lowest-priority work.
            tokio::select! {
                biased;

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
                maybe_ack = self.command_acks.recv(), if command_window.has_pending() => {
                    let Some(ack) = maybe_ack else {
                        tracing::info!("Command ack channel closed, stopping source");
                        break;
                    };
                    command_window.acknowledge(ack)?;
                }
                maybe_record = self.replays_to_execute.recv(), if can_replay => {
                    let Some(record) = maybe_record else {
                        tracing::info!("inbound channel closed");
                        return Ok(());
                    };
                    self.forward_replay(record, &output, &state_reporter).await?;
                    command_window.push(BlockCommandType::Replay);
                }
                res = self.replay_forwarder.wait_until_open(), if !gate_open => {
                    res?;
                }
                send_res = output.send(BlockCommand::Produce(ProduceCommand)), if can_produce => {
                    if send_res.is_err() {
                        tracing::info!("Command output channel closed, stopping source");
                        break;
                    }
                    command_window.push(BlockCommandType::Produce);
                    produced_blocks_count += 1;
                    // Advance watermark to the last sealed block so diff stays near 0.
                    let latest = self.block_replay_storage.latest_record();
                    if let Some(ctx) = self.block_replay_storage.get_context(latest) {
                        state_reporter.record_processed(latest, Some(ctx.timestamp), None);
                    }
                }
            }
        }

        Ok(())
    }

    async fn forward_wal_replays(
        &mut self,
        start: u64,
        end: u64,
        output: &mpsc::Sender<BlockCommand>,
    ) -> anyhow::Result<()> {
        let latest = self.block_replay_storage.latest_record();
        anyhow::ensure!(
            latest >= end,
            "Requested range end {end} exceeds latest record {latest}"
        );
        let mut command_window = CommandWindow::new(DEFAULT_COMMAND_WINDOW_CAPACITY);
        for block_num in start..=end {
            self.wait_for_command_capacity(&mut command_window, BlockCommandType::Replay)
                .await?;
            self.replay_forwarder.wait_until_open().await?;
            let record = self
                .block_replay_storage
                .get_replay_record(block_num)
                .ok_or_else(|| anyhow::anyhow!("missing replay record for block {block_num}"))?;
            let _ = self.replay_forwarder.forward(record, output).await?;
            command_window.push(BlockCommandType::Replay);
        }
        self.drain_command_window(&mut command_window).await?;
        Ok(())
    }

    async fn forward_replay(
        &self,
        record: ReplayRecord,
        output: &mpsc::Sender<BlockCommand>,
        state_reporter: &ComponentStateReporter,
    ) -> anyhow::Result<()> {
        let block_number = record.block_context.block_number;
        tracing::info!(block_number, "Received canonized block from consensus",);
        let forwarded = self.replay_forwarder.forward(record, output).await?;
        state_reporter.record_processed(forwarded.block_number, Some(forwarded.timestamp), None);
        Ok(())
    }

    async fn send_block_rebuilds(
        &mut self,
        rebuild_options: &RebuildOptions,
        last_block_in_wal: u64,
        output: &mpsc::Sender<BlockCommand>,
    ) -> anyhow::Result<()> {
        tracing::warn!(
            "Starting block rebuilds! {rebuild_options:?}, last_block_in_wal: {last_block_in_wal}"
        );
        let mut command_window = CommandWindow::new(DEFAULT_COMMAND_WINDOW_CAPACITY);
        for block_number in rebuild_options.from_block..=last_block_in_wal {
            self.wait_for_command_capacity(&mut command_window, BlockCommandType::Rebuild)
                .await?;
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
                reset_timestamp: rebuild_options.reset_timestamps,
            }));
            self.replay_forwarder.wait_until_open().await?;
            if output.send(command).await.is_err() {
                tracing::info!("Command output channel closed, stopping source");
                break;
            }
            command_window.push(BlockCommandType::Rebuild);
        }
        self.drain_command_window(&mut command_window).await?;
        Ok(())
    }

    async fn wait_for_command_capacity(
        &mut self,
        command_window: &mut CommandWindow,
        command_type: BlockCommandType,
    ) -> anyhow::Result<()> {
        while !command_window.can_send(command_type) {
            self.wait_for_command_ack(command_window).await?;
        }
        Ok(())
    }

    async fn drain_command_window(
        &mut self,
        command_window: &mut CommandWindow,
    ) -> anyhow::Result<()> {
        while command_window.has_pending() {
            self.wait_for_command_ack(command_window).await?;
        }
        Ok(())
    }

    async fn wait_for_command_ack(
        &mut self,
        command_window: &mut CommandWindow,
    ) -> anyhow::Result<()> {
        let ack = self
            .command_acks
            .recv()
            .await
            .ok_or_else(|| anyhow::anyhow!("command ack channel closed"))?;
        command_window.acknowledge(ack)
    }
}
