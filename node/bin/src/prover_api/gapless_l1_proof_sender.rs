use async_trait::async_trait;
use std::collections::BTreeMap;
use tokio::sync::mpsc;
use zksync_os_l1_sender::commands::L1SenderCommand;
use zksync_os_l1_sender::commands::prove::ProofCommand;
use zksync_os_observability::{ComponentStateReporter, GenericComponentState};
use zksync_os_pipeline::{HasBlockRangeEnd, PeekableReceiver, PipelineComponent, SendAndRecordExt};

/// Receives L1SenderCommands with ProofCommand - potentially out of order.
/// Fixes the order and sends downstream.
pub struct GaplessL1ProofSender {
    pub next_expected_batch_number: u64,
}

impl GaplessL1ProofSender {
    pub fn new(next_expected_batch_number: u64) -> Self {
        Self {
            next_expected_batch_number,
        }
    }
}

#[async_trait]
impl PipelineComponent for GaplessL1ProofSender {
    type Input = L1SenderCommand<ProofCommand>;
    type Output = L1SenderCommand<ProofCommand>;

    const COMPONENT_ID: zksync_os_pipeline::ComponentId =
        zksync_os_pipeline::ComponentId::GaplessL1ProofSender;

    async fn run(
        self,
        mut input: PeekableReceiver<Self::Input>,
        output: mpsc::UnboundedSender<Self::Output>,
        state_reporter: ComponentStateReporter,
    ) -> anyhow::Result<()> {
        let mut buffer: BTreeMap<u64, L1SenderCommand<ProofCommand>> = BTreeMap::new();
        let mut next_expected_batch_number = self.next_expected_batch_number;

        loop {
            state_reporter.enter_state(GenericComponentState::Idle);
            match input.recv().await {
                Some(command) => {
                    let arrived_batch = command.first_batch_number();
                    let arrived_last_block = command.last_block_number();
                    state_reporter.enter_state(GenericComponentState::Active);

                    if arrived_batch != next_expected_batch_number {
                        let buffer_size = buffer.len() + 1;
                        tracing::debug!(
                            "GaplessL1ProofSender: out-of-order command arrived_batch={arrived_batch} (last_block={arrived_last_block}), waiting for batch {next_expected_batch_number}, buffer_size={buffer_size}"
                        );
                    }

                    buffer.insert(arrived_batch, command);

                    // Flush ready commands
                    while let Some(next_command) = buffer.remove(&next_expected_batch_number) {
                        let flushing_batch = next_expected_batch_number;
                        let flushing_last_batch = next_command.last_batch_number();
                        let flushing_last_block = next_command.last_block_number();
                        let flushing_timestamp = next_command.block_timestamp();
                        next_expected_batch_number += next_command.batch_count() as u64;
                        tracing::debug!(
                            "GaplessL1ProofSender: sending batch {flushing_batch} (last_block={flushing_last_block}) downstream, next_expected={next_expected_batch_number}"
                        );
                        state_reporter.record_picked(
                            flushing_last_block,
                            flushing_timestamp,
                            Some(flushing_last_batch),
                        );
                        if output
                            .send_and_record(next_command, &state_reporter)
                            .is_err()
                        {
                            anyhow::bail!("Outbound channel closed");
                        }
                        state_reporter.enter_state(GenericComponentState::Active);
                    }
                }
                None => {
                    tracing::info!("inbound channel closed");
                    return Ok(());
                }
            }
        }
    }
}
