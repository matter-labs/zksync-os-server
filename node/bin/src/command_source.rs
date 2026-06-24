use anyhow::Context as _;
use async_trait::async_trait;
use std::collections::HashSet;
use tokio::sync::mpsc;
use zksync_os_backpressure::PipelineAdmissionReceiver;
use zksync_os_observability::ComponentStateReporter;
use zksync_os_pipeline::{PeekableReceiver, PipelineComponent};
use zksync_os_raft::{ConsensusRole, LeadershipSignal};
use zksync_os_sequencer::execution::block_context_provider::millis_since_epoch;
use zksync_os_sequencer::model::blocks::{BlockCommand, ProduceCommand, RebuildCommand};
use zksync_os_storage_api::{ReadReplay, ReplayRecord};

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
    /// Internal pipeline admission gate driven by backpressure monitoring.
    pub pipeline_gate: PipelineAdmissionReceiver,
    /// Current leadership status from consensus.
    pub leadership: LeadershipSignal,
}

#[derive(Debug, Clone)]
pub struct RebuildOptions {
    pub from_block_number: u64,
    pub blocks_to_empty: HashSet<u64>,
    pub reset_timestamps: bool,
}

/// External node command source.
#[derive(Debug)]
pub struct ExternalNodeCommandSource {
    pub up_to_block: Option<u64>,
    pub replays_for_sequencer: mpsc::Receiver<ReplayRecord>,
    pub pipeline_gate: PipelineAdmissionReceiver,
}

#[async_trait]
impl<Replay: ReadReplay> PipelineComponent for ConsensusNodeCommandSource<Replay> {
    type Input = ();
    type Output = BlockCommand;

    const COMPONENT_ID: zksync_os_pipeline::ComponentId =
        zksync_os_pipeline::ComponentId::ConsensusNodeCommandSource;
    // Capacity 1 is intentional: the leader arm in run_loop emits Produce tokens inside
    // tokio::select! on output.send(), firing whenever the channel has space. A larger buffer
    // would let the leader queue multiple tokens ahead of execution. Capacity of 1 ensures
    // at most one un-executed Produce command in flight, making the downstream consumer the pacer.
    const OUTPUT_CHANNEL_CAPACITY: usize = 1;

    async fn run(
        mut self,
        _input: PeekableReceiver<()>,
        output: mpsc::Sender<BlockCommand>,
        state_reporter: ComponentStateReporter,
    ) -> anyhow::Result<()> {
        let last_block_in_wal = self.block_replay_storage.latest_record();

        let replay_until = if let Some(rebuild_options) = &self.rebuild_options {
            assert!(
                rebuild_options.from_block_number >= self.starting_block,
                "rebuild_from_block_number must be >= starting_block, got {} < {}",
                rebuild_options.from_block_number,
                self.starting_block
            );
            assert!(
                rebuild_options.from_block_number <= last_block_in_wal,
                "rebuild_from_block_number must be <= last_block_in_wal, got {} > {}",
                rebuild_options.from_block_number,
                last_block_in_wal
            );
            rebuild_options.from_block_number - 1
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
    const MAX_REPLAYS_TO_DRAIN_PER_LOOP: usize = 32;

    /// This method kicks in after all local canonized Replayed Records (WAL) are replayed.
    /// Produces `Produce` commands only when the node is the leader.
    async fn run_loop(
        mut self,
        output: mpsc::Sender<BlockCommand>,
        state_reporter: ComponentStateReporter,
    ) -> anyhow::Result<()> {
        let mut leadership = self.leadership.clone();
        let mut role = leadership.current_role();
        tracing::info!(?role, "Consensus role initialized");

        loop {
            // Drain any already-queued canonized replays while the gate is open.
            for _ in 0..Self::MAX_REPLAYS_TO_DRAIN_PER_LOOP {
                if !self.pipeline_gate.is_open() {
                    break;
                }
                match self.replays_to_execute.try_recv() {
                    Ok(record) => {
                        if !Self::forward_replay(record, &output, &state_reporter).await? {
                            return Ok(());
                        }
                    }
                    Err(mpsc::error::TryRecvError::Empty) => break,
                    Err(mpsc::error::TryRecvError::Disconnected) => {
                        tracing::info!("inbound channel closed");
                        return Ok(());
                    }
                }
            }

            // Read the gate after draining so the select guards below see the
            // post-drain state. The gate may still flip while we are parked in
            // select! with the recv/produce arms enabled; that bounded one-block
            // overshoot is acceptable for soft backpressure.
            let gate_open = self.pipeline_gate.is_open();
            let can_produce = role == ConsensusRole::Leader && gate_open;

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
                maybe_record = self.replays_to_execute.recv(), if gate_open => {
                    let Some(record) = maybe_record else {
                        tracing::info!("inbound channel closed");
                        return Ok(());
                    };
                    if !Self::forward_replay(record, &output, &state_reporter).await? {
                        return Ok(());
                    }
                }
                _ = self.pipeline_gate.wait_until_open(), if !gate_open => {}
                send_res = output.send(BlockCommand::Produce(ProduceCommand)), if can_produce => {
                    if send_res.is_err() {
                        tracing::info!("Command output channel closed, stopping source");
                        break;
                    }
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
        for block_num in start..=end {
            self.pipeline_gate.wait_until_open().await;
            let record = self
                .block_replay_storage
                .get_replay_record(block_num)
                .with_context(|| format!("missing replay record for block {block_num}"))?;
            if output
                .send(BlockCommand::Replay(Box::new(record)))
                .await
                .is_err()
            {
                tracing::info!("Command output channel closed, stopping WAL replay");
                return Ok(());
            }
        }
        Ok(())
    }

    /// Returns `false` if the output channel has closed (caller should stop).
    async fn forward_replay(
        record: ReplayRecord,
        output: &mpsc::Sender<BlockCommand>,
        state_reporter: &ComponentStateReporter,
    ) -> anyhow::Result<bool> {
        let block_number = record.block_context.block_number;
        let timestamp = record.block_context.timestamp;
        tracing::info!(block_number, "Received canonized block from consensus");
        if output
            .send(BlockCommand::Replay(Box::new(record)))
            .await
            .is_err()
        {
            tracing::info!("Command output channel closed, stopping source");
            return Ok(false);
        }
        state_reporter.record_processed(block_number, Some(timestamp), None);
        Ok(true)
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
        for block_number in rebuild_options.from_block_number..=last_block_in_wal {
            self.pipeline_gate.wait_until_open().await;
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
            if output.send(command).await.is_err() {
                tracing::info!("Command output channel closed, stopping source");
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

    const COMPONENT_ID: zksync_os_pipeline::ComponentId =
        zksync_os_pipeline::ComponentId::ExternalNodeCommandSource;
    const OUTPUT_CHANNEL_CAPACITY: usize = 5;

    async fn run(
        mut self,
        _input: PeekableReceiver<()>,
        output: mpsc::Sender<BlockCommand>,
        state_reporter: ComponentStateReporter,
    ) -> anyhow::Result<()> {
        loop {
            self.pipeline_gate.wait_until_open().await;
            let Some(record) = self.replays_for_sequencer.recv().await else {
                break;
            };
            let block_number = record.block_context.block_number;
            let timestamp = record.block_context.timestamp;
            let txs = record.transactions.len();
            let force_preimages = record.force_preimages.len();
            let force_preimage_bytes = record
                .force_preimages
                .iter()
                .map(|(_, value)| value.len())
                .sum::<usize>();
            let protocol_version = record.protocol_version.to_string();
            let starting_l1_priority_id = record.starting_cursors.l1_priority_id;
            let command = BlockCommand::Replay(Box::new(record));
            tracing::info!(
                "Received replay block command from main node: block_number: {block_number}, \
                 txs: {txs}, force_preimages: {force_preimages}, \
                 force_preimage_bytes: {force_preimage_bytes}, protocol_version: {protocol_version}, \
                 starting_l1_priority_id: {starting_l1_priority_id}"
            );
            tracing::debug!(?command, "Received replay block command from main node");

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
                tracing::info!("Command output channel closed, stopping source");
                break;
            }
            state_reporter.record_processed(block_number, Some(timestamp), None);
        }

        Ok(())
    }
}
