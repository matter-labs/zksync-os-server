use crate::{CommandWindow, DEFAULT_COMMAND_WINDOW_CAPACITY, ReplayCommandForwarder};
use async_trait::async_trait;
use tokio::sync::mpsc;
use zksync_os_observability::ComponentStateReporter;
use zksync_os_pipeline::{PeekableReceiver, PipelineComponent};
use zksync_os_sequencer::model::blocks::{BlockCommand, BlockCommandType, CommandAck};
use zksync_os_storage_api::ReplayRecord;

/// External node command source.
#[derive(Debug)]
pub struct ExternalNodeCommandSource {
    pub up_to_block: Option<u64>,
    pub replays_for_sequencer: mpsc::Receiver<ReplayRecord>,
    pub command_acks: mpsc::Receiver<CommandAck>,
    pub replay_forwarder: ReplayCommandForwarder,
}

#[async_trait]
impl PipelineComponent for ExternalNodeCommandSource {
    type Input = ();
    type Output = BlockCommand;

    const COMPONENT_ID: zksync_os_pipeline::ComponentId =
        zksync_os_pipeline::ComponentId::ExternalNodeCommandSource;

    async fn run(
        mut self,
        _input: PeekableReceiver<()>,
        output: mpsc::Sender<BlockCommand>,
        state_reporter: ComponentStateReporter,
    ) -> anyhow::Result<()> {
        let mut command_window = CommandWindow::new(DEFAULT_COMMAND_WINDOW_CAPACITY);
        loop {
            let gate_open = self.replay_forwarder.is_open();
            let can_replay = gate_open && command_window.can_send(BlockCommandType::Replay);

            tokio::select! {
                biased;

                maybe_ack = self.command_acks.recv(), if command_window.has_pending() => {
                    let Some(ack) = maybe_ack else {
                        tracing::info!("Command ack channel closed, stopping external source");
                        break;
                    };
                    command_window.acknowledge(ack)?;
                }
                res = self.replay_forwarder.wait_until_open(), if !gate_open => {
                    res?;
                }
                maybe_record = self.replays_for_sequencer.recv(), if can_replay => {
                    let Some(record) = maybe_record else {
                        while command_window.has_pending() {
                            let ack = self
                                .command_acks
                                .recv()
                                .await
                                .ok_or_else(|| anyhow::anyhow!("command ack channel closed"))?;
                            command_window.acknowledge(ack)?;
                        }
                        break;
                    };
                    let block_number = record.block_context.block_number;
                    let txs = record.transactions.len();
                    let force_preimages = record.force_preimages.len();
                    let force_preimage_bytes = record
                        .force_preimages
                        .iter()
                        .map(|(_, value)| value.len())
                        .sum::<usize>();
                    let protocol_version = record.protocol_version.to_string();
                    let starting_l1_priority_id = record.starting_cursors.l1_priority_id;
                    tracing::info!(
                        "Received replay block command from main node: block_number: {block_number}, \
                         txs: {txs}, force_preimages: {force_preimages}, \
                         force_preimage_bytes: {force_preimage_bytes}, protocol_version: {protocol_version}, \
                         starting_l1_priority_id: {starting_l1_priority_id}"
                    );
                    tracing::debug!(?record, "Received replay block command from main node");

                    if let Some(up_to_block) = self.up_to_block
                        && block_number > up_to_block
                    {
                        tracing::info!(
                            up_to_block,
                            "Reached up_to_block, halting external command source"
                        );
                        futures::future::pending::<()>().await;
                    }

                    let forwarded = self.replay_forwarder.forward(record, &output).await?;
                    state_reporter.record_processed(
                        forwarded.block_number,
                        Some(forwarded.timestamp),
                        None,
                    );
                    command_window.push(BlockCommandType::Replay);
                }
            }
        }

        Ok(())
    }
}
