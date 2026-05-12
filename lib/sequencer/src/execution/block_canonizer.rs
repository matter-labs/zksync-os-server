use crate::execution::metrics::BlockCanonizerState;
use crate::model::blocks::{BlockCommandType, BlockPayload};
use async_trait::async_trait;
use std::collections::VecDeque;
use tokio::sync::mpsc;
use zksync_os_observability::ComponentStateReporter;
use zksync_os_pipeline::{PeekableReceiver, PipelineComponent, SendAndRecordExt};
use zksync_os_storage_api::ReplayRecord;

/// Pipeline component that ensures that only canonized blocks are sent downstream,
///  effectively serving as a canonization fence.
/// Assumes that all **Replay** commands from upstream are already canonized:
/// they are either:
///         from local storage (replayed on startup)
///         or are produced by some other node - thus already canonized by the consensus protocol
/// **Produce** (proposed) commands are first waiting the canonization
///  (This component sends them to Consensus and wait for them to return as Replays).
///
/// This component doesn't rely on or track the node role (leader vs replica) -
/// it can handle both Produce and Replay upstream commands.
pub struct BlockCanonizer<Consensus>
where
    Consensus: BlockCanonization,
{
    pub consensus: Consensus,
    /// Channel to send new canonized blocks to for the node to replay.
    /// They are sent to `NodeCommandSource` and then through the whole pipeline.
    pub canonized_blocks_for_execution: mpsc::Sender<ReplayRecord>,
}

#[async_trait]
pub trait BlockCanonization: Send + 'static {
    async fn propose(&self, record: ReplayRecord) -> anyhow::Result<()>;
    async fn next_canonized(&mut self) -> anyhow::Result<ReplayRecord>;
}

/// Degenerate consensus implementation - just an async channel to itself.
pub struct NoopCanonization {
    pub sender: mpsc::UnboundedSender<ReplayRecord>,
    pub receiver: mpsc::UnboundedReceiver<ReplayRecord>,
}

impl NoopCanonization {
    pub fn new() -> Self {
        let (sender, receiver) = mpsc::unbounded_channel();
        Self { sender, receiver }
    }
}

impl Default for NoopCanonization {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl BlockCanonization for NoopCanonization {
    async fn propose(&self, record: ReplayRecord) -> anyhow::Result<()> {
        self.sender.send(record)?;
        Ok(())
    }

    async fn next_canonized(&mut self) -> anyhow::Result<ReplayRecord> {
        self.receiver
            .recv()
            .await
            .ok_or_else(|| anyhow::anyhow!("consensus replay channel closed"))
    }
}

#[async_trait]
impl<Consensus> PipelineComponent for BlockCanonizer<Consensus>
where
    Consensus: BlockCanonization,
{
    /// Input from BlockExecutor
    type Input = BlockPayload;
    /// Output to BlockApplier
    type Output = BlockPayload;

    const COMPONENT_ID: zksync_os_pipeline::ComponentId =
        zksync_os_pipeline::ComponentId::BlockCanonizer;

    async fn run(
        mut self,
        mut input: PeekableReceiver<Self::Input>,
        output: mpsc::Sender<Self::Output>,
        state_reporter: ComponentStateReporter,
    ) -> anyhow::Result<()> {
        /// Maximum number of blocks that can be waiting for canonization.
        /// When this limit is reached, backpressure is applied to the upstream BlockExecutor.
        const MAX_PRODUCED_QUEUE_SIZE: usize = 2;

        let mut produced_queue: VecDeque<BlockPayload> = VecDeque::new();

        loop {
            if produced_queue.len() >= MAX_PRODUCED_QUEUE_SIZE {
                state_reporter.enter_state(BlockCanonizerState::ProducedQueueFull);
            } else {
                state_reporter.enter_state(BlockCanonizerState::Idle);
            }
            tokio::select! {
                // Select arm that receives canonized blocks from Consensus.
                // If this block was earlier proposed by this node - sends downstream.
                // Otherwise - sends to the beginning of pipeline for execution.
                canonized = self.consensus.next_canonized() => {
                    let record = canonized?;
                    state_reporter.enter_state(BlockCanonizerState::HandlingConsensusBlock);
                    if let Some(BlockPayload {
                        output: block_output,
                        record: produced_replay,
                        command_type: cmd_type,
                    }) = produced_queue.pop_front()
                    {
                        tracing::info!(
                            "Received a Replay block {} (block output hash: {}) from Consensus while having a pending block. \
                            Matching with locally produced block and sending downstream for persistence. \
                            additional pending blocks in the queue: {}",
                            record.block_context.block_number,
                            record.block_output_hash,
                            produced_queue.len(),
                        );
                        if produced_replay != record {
                            anyhow::bail!(
                                "canonized replay record mismatch at block {}. \
                                Other node became the leader?",
                                produced_replay.block_context.block_number
                            );
                        }
                        output
                            .send_and_record(
                                BlockPayload {
                                    output: block_output,
                                    record: produced_replay,
                                    command_type: cmd_type,
                                },
                                &state_reporter,
                            )?;
                    } else {
                        tracing::info!(
                            "Received new block {} (block output hash: {}) from Consensus. \
                            Sending as Replay command to the pipeline beginning.",
                            record.block_context.block_number,
                            record.block_output_hash,
                        );

                        self.canonized_blocks_for_execution.send(record).await?;
                    }
                }
                // Select arm that receives executed blocks from `BlockExecutor` (upstream).
                // Only receive when we have capacity in the produced_queue.
                maybe_executed = input.recv(), if produced_queue.len() < MAX_PRODUCED_QUEUE_SIZE => {
                    let Some(BlockPayload {
                        output: block_output,
                        record: replay_record,
                        command_type: cmd_type,
                    }) = maybe_executed
                    else {
                        tracing::info!("inbound channel closed");
                        return Ok(());
                    };
                    state_reporter.enter_state(BlockCanonizerState::HandlingExecutorBlock);
                    state_reporter.record_picked(
                        replay_record.block_context.block_number,
                        Some(replay_record.block_context.timestamp),
                        None,
                    );
                    match cmd_type {
                        BlockCommandType::Replay => {
                        tracing::info!(
                            "Received a Replay block {} (block output hash: {}) from BlockExecutor. \
                            Sending downstream.",
                            replay_record.block_context.block_number,
                            replay_record.block_output_hash,
                        );
                        output
                            .send_and_record(
                                BlockPayload {
                                    output: block_output,
                                    record: replay_record,
                                    command_type: cmd_type,
                                },
                                &state_reporter,
                            )?;
                        }
                        BlockCommandType::Produce | BlockCommandType::Rebuild => {
                            tracing::info!(
                                "Received a {:?} block {} (block output hash: {}) from BlockExecutor. \
                                Sending to consensus for canonization.",
                                cmd_type,
                                replay_record.block_context.block_number,
                                replay_record.block_output_hash,
                            );
                            state_reporter
                                .enter_state(BlockCanonizerState::ProposingToConsensus);
                            let proposed = replay_record.clone();
                            self.consensus.propose(proposed).await?;
                            produced_queue.push_back(BlockPayload {
                                output: block_output,
                                record: replay_record,
                                command_type: cmd_type,
                            });
                        }
                    }
                }
            }
        }
    }
}
