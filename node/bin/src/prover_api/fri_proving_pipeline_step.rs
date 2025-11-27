use super::proof_storage::ProofStorage;
use crate::prover_api::fri_job_manager::FriJobManager;
use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use zksync_os_l1_sender::batcher_model::{FriProof, ProverInput, SignedBatchEnvelope};
use zksync_os_pipeline::{PeekableReceiver, PipelineComponent};

/// Pipeline step that waits for batches to be FRI proved.
///
/// This component:
/// - Receives batches with ProverInput from the batcher
/// - Adds them directly to FriJobManager (which makes them available via HTTP API)
/// - Receives proofs from FriJobManager (submitted via HTTP API or fake provers)
/// - Forwards the proofs downstream in the pipeline
///
/// The FriJobManager itself is purely reactive (no run loop), accessed/driven by:
/// - HTTP server (provers call pick_next_job, submit_proof, etc.)
/// - Fake provers pool
/// - This pipeline step (adds jobs via add_job)
pub struct FriProvingPipelineStep {
    fri_job_manager: Arc<FriJobManager>,
    batches_with_proof_receiver: mpsc::Receiver<SignedBatchEnvelope<FriProof>>,
}

impl FriProvingPipelineStep {
    pub fn new(
        proof_storage: ProofStorage,
        assignment_timeout: Duration,
        max_assigned_batch_range: usize,
    ) -> (Self, Arc<FriJobManager>) {
        // Create channel for completed proofs - between FriProveManager and GaplessCommitter
        let (batches_with_proof_sender, batches_with_proof_receiver) =
            mpsc::channel::<SignedBatchEnvelope<FriProof>>(5);

        let fri_job_manager = Arc::new(FriJobManager::new(
            batches_with_proof_sender,
            proof_storage,
            assignment_timeout,
            max_assigned_batch_range,
        ));

        let result = Self {
            fri_job_manager: fri_job_manager.clone(),
            batches_with_proof_receiver,
        };

        (result, fri_job_manager)
    }
}

#[async_trait]
impl PipelineComponent for FriProvingPipelineStep {
    type Input = SignedBatchEnvelope<ProverInput>;
    type Output = SignedBatchEnvelope<FriProof>;

    const NAME: &'static str = "fri_proving";
    const OUTPUT_BUFFER_SIZE: usize = 5;

    async fn run(
        mut self,
        mut input: PeekableReceiver<Self::Input>,
        output: mpsc::Sender<Self::Output>,
    ) -> anyhow::Result<()> {
        // Forward batches: pipeline input → FriJobManager (add_job) → pipeline output (via proofs channel)
        // Two concurrent tasks handle the bidirectional flow
        tokio::select! {
            result = async {
                while let Some(batch) = input.recv().await {
                    tracing::info!(
                        "Received batch for FRI proving: {:?}",
                        batch.batch_number()
                    );
                    // Add job directly to FriJobManager - this will await if queue is full
                    self.fri_job_manager.add_job(batch).await
                }
                Ok::<(), anyhow::Error>(())
            } => {
                result?;
                anyhow::bail!("FRI proving input stream ended unexpectedly")
            },
            _ = async {
                while let Some(proof) = self.batches_with_proof_receiver.recv().await {
                    tracing::info!(
                        "Received batch after FRI proving: {:?}",
                        proof.batch_number()
                    );
                    let _ = output.send(proof).await;
                }
            } => anyhow::bail!("FRI proving output stream ended unexpectedly"),
        }
    }
}
