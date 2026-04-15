use async_trait::async_trait;
use std::collections::BTreeMap;
use zksync_os_l1_sender::commands::L1SenderCommand;
use zksync_os_l1_sender::commands::prove::ProofCommand;
use zksync_os_observability::{ComponentHealthReporter, GenericComponentState};
use zksync_os_pipeline::{PipelineComponent, TrackedUnboundedReceiver, TrackedUnboundedSender};

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
        mut input: TrackedUnboundedReceiver<Self::Input>,
        output: TrackedUnboundedSender<Self::Output>,
    ) -> anyhow::Result<()> {
        let mut buffer: BTreeMap<u64, L1SenderCommand<ProofCommand>> = BTreeMap::new();
        let mut next_expected_batch_number = self.next_expected_batch_number;

        loop {
            self.health_reporter.enter_state(GenericComponentState::Idle);
            match input.recv().await {
                Some(command) => {
                    self.health_reporter.enter_state(GenericComponentState::Active);

                    buffer.insert(command.first_batch_number(), command);

                    // Flush ready commands
                    while let Some(next_command) = buffer.remove(&next_expected_batch_number) {
                        next_expected_batch_number += next_command.batch_count() as u64;
                        if output
                            .send_and_record(next_command, &self.health_reporter)
                            .is_err()
                        {
                            anyhow::bail!("Outbound channel closed");
                        }
                        self.health_reporter.enter_state(GenericComponentState::Active);
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
