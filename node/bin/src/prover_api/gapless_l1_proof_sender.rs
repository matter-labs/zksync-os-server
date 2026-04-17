use async_trait::async_trait;
use std::collections::BTreeMap;
use tokio::sync::mpsc;
use zksync_os_l1_sender::commands::L1SenderCommand;
use zksync_os_l1_sender::commands::prove::ProofCommand;
use zksync_os_observability::{ComponentHealthReporter, GenericComponentState};
use zksync_os_pipeline::{HasBlockRangeEnd, PeekableReceiver, PipelineComponent, SendAndRecordExt};

/// Receives L1SenderCommands with ProofCommand - potentially out of order.
/// Fixes the order and sends downstream.
pub struct GaplessL1ProofSender {
    pub next_expected_batch_number: u64,
    pub health_reporter: ComponentHealthReporter,
}

impl GaplessL1ProofSender {
    pub fn new(next_expected_batch_number: u64, health_reporter: ComponentHealthReporter) -> Self {
        Self {
            next_expected_batch_number,
            health_reporter,
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
    ) -> anyhow::Result<()> {
        let mut buffer: BTreeMap<u64, L1SenderCommand<ProofCommand>> = BTreeMap::new();
        let mut next_expected_batch_number = self.next_expected_batch_number;

        loop {
            self.health_reporter
                .enter_state(GenericComponentState::Idle);
            match input.recv().await {
                Some(command) => {
                    let arrived_batch = command.first_batch_number();
                    let arrived_last_block = command.last_block_number();
                    self.health_reporter
                        .record_picked(arrived_last_block, command.block_timestamp());
                    self.health_reporter.record_batch_picked(arrived_batch);
                    self.health_reporter
                        .enter_state(GenericComponentState::Active);

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
                        let flushing_last_block = next_command.last_block_number();
                        next_expected_batch_number += next_command.batch_count() as u64;
                        tracing::debug!(
                            "GaplessL1ProofSender: sending batch {flushing_batch} (last_block={flushing_last_block}) downstream, next_expected={next_expected_batch_number}"
                        );
                        if output
                            .send_and_record(next_command, &self.health_reporter)
                            .is_err()
                        {
                            anyhow::bail!("Outbound channel closed");
                        }
                        self.health_reporter
                            .enter_state(GenericComponentState::Active);
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
