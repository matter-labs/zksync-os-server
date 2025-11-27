use async_trait::async_trait;
use std::collections::BTreeMap;
use tokio::sync::mpsc;
use zksync_os_l1_sender::commands::L1SenderCommand;
use zksync_os_l1_sender::commands::prove::ProofCommand;
use zksync_os_observability::{ComponentStateReporter, GenericComponentState};
use zksync_os_pipeline::{PeekableReceiver, PipelineComponent};

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

    const NAME: &'static str = "gapless_l1_proof_sender";
    const OUTPUT_BUFFER_SIZE: usize = 5;

    async fn run(
        self,
        mut input: PeekableReceiver<Self::Input>,
        output: mpsc::Sender<Self::Output>,
    ) -> anyhow::Result<()> {
        let latency_tracker = ComponentStateReporter::global().handle_for(
            "gapless_l1_proof_sender",
            GenericComponentState::WaitingRecv,
        );

        let mut buffer: BTreeMap<u64, L1SenderCommand<ProofCommand>> = BTreeMap::new();
        let mut next_expected_batch_number = self.next_expected_batch_number;

        loop {
            latency_tracker.enter_state(GenericComponentState::WaitingRecv);
            match input.recv().await {
                Some(command) => {
                    latency_tracker.enter_state(GenericComponentState::Processing);

                    buffer.insert(command.first_batch_number(), command);

                    // Flush ready commands
                    while let Some(next_command) = buffer.remove(&next_expected_batch_number) {
                        next_expected_batch_number += next_command.batch_count() as u64;
                        latency_tracker.enter_state(GenericComponentState::WaitingSend);
                        output.send(next_command).await?;
                        latency_tracker.enter_state(GenericComponentState::Processing);
                    }
                }
                None => {
                    anyhow::bail!("GaplessL1ProofSender input stream ended unexpectedly");
                }
            }
        }
    }
}
