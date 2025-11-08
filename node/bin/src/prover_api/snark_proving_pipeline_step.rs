use super::snark_job_manager::SnarkJobManager;
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::mpsc;
use zksync_os_l1_sender::batcher_model::{FriProof, SignedBatchEnvelope};
use zksync_os_l1_sender::commands::prove::ProofCommand;
use zksync_os_pipeline::{PeekableReceiver, PipelineComponent};

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
    batches_for_prove_sender: mpsc::Sender<SignedBatchEnvelope<FriProof>>,
    proof_commands_receiver: mpsc::Receiver<ProofCommand>,
}

impl SnarkProvingPipelineStep {
    pub fn new(max_fris_per_snark: usize) -> (Self, Arc<SnarkJobManager>) {
        // Create channels for SnarkJobManager
        // IMPORTANT: capacity `max_fris_per_snark` to allow SnarkJobManager
        //            to group multiple batches (FRI proofs) to a single SNARK
        //            (on `pick_snark_job` request, it consumes messages from the inbound channel up until `max_fris_per_snark`)
        let (batches_for_prove_sender, batches_for_prove_receiver) =
            mpsc::channel::<SignedBatchEnvelope<FriProof>>(max_fris_per_snark);

        let (proof_commands_sender, proof_commands_receiver) = mpsc::channel::<ProofCommand>(1);

        let snark_job_manager = Arc::new(SnarkJobManager::new(
            PeekableReceiver::new(batches_for_prove_receiver),
            proof_commands_sender,
            max_fris_per_snark,
        ));

        let result = Self {
            batches_for_prove_sender,
            proof_commands_receiver,
        };

        (result, snark_job_manager)
    }
}

#[async_trait]
impl PipelineComponent for SnarkProvingPipelineStep {
    type Input = SignedBatchEnvelope<FriProof>;
    type Output = ProofCommand;

    const NAME: &'static str = "snark_proving";
    const OUTPUT_BUFFER_SIZE: usize = 5;

    async fn run(
        mut self,
        mut input: PeekableReceiver<Self::Input>,
        output: mpsc::Sender<Self::Output>,
    ) -> anyhow::Result<()> {
        // Forward batches: pipeline input → SnarkJobManager → pipeline output
        // Two concurrent tasks handle the bidirectional flow
        tokio::select! {
            _ = async {
                while let Some(batch) = input.recv().await {
                    let _ = self.batches_for_prove_sender.send(batch).await;
                }
            } => anyhow::bail!("SNARK proving input stream ended unexpectedly"),
            _ = async {
                while let Some(proof_command) = self.proof_commands_receiver.recv().await {
                    let _ = output.send(proof_command).await;
                }
            } => anyhow::bail!("SNARK proving output stream ended unexpectedly"),
        }
    }
}
