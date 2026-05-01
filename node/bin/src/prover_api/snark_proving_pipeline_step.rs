use super::snark_job_manager::SnarkJobManager;
use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use zksync_os_batch_types::batcher_model::{FriProof, SignedBatchEnvelope};
use zksync_os_l1_sender::commands::L1SenderCommand;
use zksync_os_l1_sender::commands::prove::ProofCommand;
use zksync_os_observability::ComponentStateReporter;
use zksync_os_pipeline::{PeekableReceiver, PipelineComponent, SendAndRecordExt};

/// Pipeline step that waits for batches to be SNARK proved.
///
/// This component:
/// - Receives batches with FRI proofs (after they are committed to L1)
/// - Forwards them to SnarkJobManager (which makes them available via HTTP API)
/// - Receives batches with proofs from SnarkJobManager (submitted via HTTP API or fake provers)
/// - Forwards the proof commands downstream to L1 proof sender
///
/// The SnarkJobManager itself is purely reactive (no run loop), accessed/driven by:
/// - HTTP server (provers call pick_next_job, submit_proof, etc.)
/// - Fake provers pool
pub struct SnarkProvingPipelineStep {
    last_proved_batch_number: u64,
    snark_job_manager: Arc<SnarkJobManager>,
    proof_commands_receiver: mpsc::Receiver<ProofCommand>,
}

impl SnarkProvingPipelineStep {
    pub fn new(
        max_fris_per_snark: usize,
        last_proved_batch_number: u64,
        assignment_timeout: Duration,
        max_assigned_batch_range: usize,
    ) -> (Self, Arc<SnarkJobManager>) {
        let (proof_commands_sender, proof_commands_receiver) = mpsc::channel::<ProofCommand>(1);

        let snark_job_manager = Arc::new(SnarkJobManager::new(
            proof_commands_sender,
            max_fris_per_snark,
            assignment_timeout,
            max_assigned_batch_range,
        ));

        let result = Self {
            last_proved_batch_number,
            snark_job_manager: snark_job_manager.clone(),
            proof_commands_receiver,
        };

        (result, snark_job_manager)
    }
}

#[async_trait]
impl PipelineComponent for SnarkProvingPipelineStep {
    type Input = SignedBatchEnvelope<FriProof>;
    type Output = L1SenderCommand<ProofCommand>;

    const COMPONENT_ID: zksync_os_pipeline::ComponentId =
        zksync_os_pipeline::ComponentId::SnarkJobManager;

    async fn run(
        mut self,
        mut input: PeekableReceiver<Self::Input>,
        output: mpsc::Sender<Self::Output>,
        state_reporter: ComponentStateReporter,
    ) -> anyhow::Result<()> {
        // Forward batches: pipeline input → SnarkJobManager → pipeline output
        // Two concurrent tasks handle the bidirectional flow
        tokio::select! {
            result = async {
                while let Some(batch) = input.recv_and_record_picked(&state_reporter).await {
                    if batch.batch_number() > self.last_proved_batch_number {
                        self.snark_job_manager.add_job(batch).await;
                    } else {
                        let passthrough = L1SenderCommand::Passthrough(Box::new(batch));
                        output.send_and_record(passthrough, &state_reporter)?;
                    }
                }
                Ok::<(), anyhow::Error>(())
            } => {
                result?;
                tracing::info!("inbound channel closed");
                return Ok(());
            },
            result = async {
                while let Some(proof_command) = self.proof_commands_receiver.recv().await {
                    output.send_and_record(
                        L1SenderCommand::SendToL1(proof_command),
                        &state_reporter,
                    )?;
                }
                Ok::<(), anyhow::Error>(())
            } => {
                result?;
                tracing::info!("outbound channel closed");
                return Ok(());
            },
        }
    }
}
