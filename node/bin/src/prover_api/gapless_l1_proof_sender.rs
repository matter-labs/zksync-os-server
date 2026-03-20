use async_trait::async_trait;
use std::collections::BTreeMap;
use tokio::sync::mpsc;
use zksync_os_l1_sender::commands::L1SenderCommand;
use zksync_os_l1_sender::commands::prove::ProofCommand;
use zksync_os_observability::{ComponentHealthReporter, GenericComponentState};
use zksync_os_pipeline::{PeekableReceiver, PipelineComponent};

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

    const NAME: &'static str = "gapless_l1_proof_sender";
    const OUTPUT_BUFFER_SIZE: usize = 5;

    async fn run(
        self,
        mut input: PeekableReceiver<Self::Input>,
        output: mpsc::Sender<Self::Output>,
    ) -> anyhow::Result<()> {
        let health_reporter = self.health_reporter;

        let mut buffer: BTreeMap<u64, L1SenderCommand<ProofCommand>> = BTreeMap::new();
        let mut next_expected_batch_number = self.next_expected_batch_number;

        loop {
            health_reporter.enter_state(GenericComponentState::WaitingRecv);
            match input.recv().await {
                Some(command) => {
                    health_reporter.enter_state(GenericComponentState::Processing);

                    buffer.insert(command.first_batch_number(), command);

                    // Flush ready commands
                    while let Some(next_command) = buffer.remove(&next_expected_batch_number) {
                        let last_block = next_command.last_block_number();
                        next_expected_batch_number += next_command.batch_count() as u64;
                        health_reporter.enter_state(GenericComponentState::WaitingSend);
                        output.send(next_command).await?;
                        health_reporter.record_processed(last_block);
                        health_reporter.enter_state(GenericComponentState::Processing);
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
