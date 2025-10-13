use super::fri_job_manager::FriJobManager;
use super::proof_storage::ProofStorage;
use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use zksync_os_l1_sender::batcher_model::{BatchEnvelope, FriProof, ProverInput};
use zksync_os_pipeline::{PeekableReceiver, PipelineComponent};

/// Pipeline step that waits for batches to be FRI proved.
///
/// This component:
/// - Receives batches with ProverInput from the batcher
/// - Forwards them to FriJobManager (which makes them available via HTTP API)
/// - Receives proofs from FriJobManager (submitted via HTTP API or fake provers)
/// - Forwards the proofs downstream in the pipeline
///
/// The FriJobManager itself is purely reactive (no run loop), accessed/driven by:
/// - HTTP server (provers call pick_next_job, submit_proof, etc.)
/// - Fake provers pool
pub struct FriProvingPipelineStep {
    batches_for_prove_sender: mpsc::Sender<BatchEnvelope<ProverInput>>,
    batches_with_proof_receiver: mpsc::Receiver<BatchEnvelope<FriProof>>,
}

impl FriProvingPipelineStep {
    pub fn new(
        proof_storage: ProofStorage,
        assignment_timeout: Duration,
        max_assigned_batch_range: usize,
    ) -> (Self, Arc<FriJobManager>) {
        // Create channels for FriJobManager
        // Capacity: 1 - we don't want to add additional buffers here -
        // they are defined uniformly in `OUTPUT_BUFFER_SIZE` const of pipeline steps.
        let (batches_for_prove_sender, batches_for_prove_receiver) =
            mpsc::channel::<BatchEnvelope<ProverInput>>(1);

        let (batches_with_proof_sender, batches_with_proof_receiver) =
            mpsc::channel::<BatchEnvelope<FriProof>>(1);

        let fri_job_manager = Arc::new(FriJobManager::new(
            batches_for_prove_receiver,
            batches_with_proof_sender,
            proof_storage,
            assignment_timeout,
            max_assigned_batch_range,
        ));

        let result = Self {
            batches_for_prove_sender,
            batches_with_proof_receiver,
        };

        (result, fri_job_manager)
    }
}

#[async_trait]
impl PipelineComponent for FriProvingPipelineStep {
    type Input = BatchEnvelope<ProverInput>;
    type Output = BatchEnvelope<FriProof>;

    const NAME: &'static str = "fri_proving";
    const OUTPUT_BUFFER_SIZE: usize = 5;

    async fn run(
        mut self,
        mut input: PeekableReceiver<Self::Input>,
        output: mpsc::Sender<Self::Output>,
    ) -> anyhow::Result<()> {
        // Forward batches: pipeline input → FriJobManager → pipeline output
        // Two concurrent tasks handle the bidirectional flow
        tokio::select! {
            _ = async {
                while let Some(batch) = input.recv().await {
                    let _ = self.batches_for_prove_sender.send(batch).await;
                }
            } => anyhow::bail!("FRI proving input stream ended unexpectedly"),
            _ = async {
                while let Some(proof) = self.batches_with_proof_receiver.recv().await {
                    let _ = output.send(proof).await;
                }
            } => anyhow::bail!("FRI proving output stream ended unexpectedly"),
        }
    }
}
