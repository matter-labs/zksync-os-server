use super::proof_storage::ProofStorage;
use crate::prover_api::fri_job_manager::FriJobManager;
use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use zksync_os_batch_types::batcher_model::{FriProof, ProverInput, SignedBatchEnvelope};
use zksync_os_observability::ComponentStateReporter;
use zksync_os_pipeline::{PeekableReceiver, PipelineComponent, SendAndRecordExt};

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
    last_proved_batch_number: u64,
    fri_job_manager: Arc<FriJobManager>,
    batches_with_proof_receiver: mpsc::UnboundedReceiver<SignedBatchEnvelope<FriProof>>,
}

impl FriProvingPipelineStep {
    pub fn new(
        proof_storage: ProofStorage,
        last_proved_batch_number: u64,
        assignment_timeout: Duration,
        max_assigned_batch_range: usize,
    ) -> (Self, Arc<FriJobManager>) {
        // Internal channel from FriJobManager submissions to the forwarding select-arm below.
        // Unbounded: the sole in-flight bound for this stage is ProverJobMap::max_assigned_batch_range,
        // which blocks add_job. A bounded buffer here would only create spurious 500s at submit.
        let (batches_with_proof_sender, batches_with_proof_receiver) =
            mpsc::unbounded_channel::<SignedBatchEnvelope<FriProof>>();

        let fri_job_manager = Arc::new(FriJobManager::new(
            batches_with_proof_sender,
            proof_storage,
            assignment_timeout,
            max_assigned_batch_range,
        ));

        let result = Self {
            last_proved_batch_number,
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

    const COMPONENT_ID: zksync_os_pipeline::ComponentId =
        zksync_os_pipeline::ComponentId::FriJobManager;

    async fn run(
        mut self,
        mut input: PeekableReceiver<Self::Input>,
        output: mpsc::UnboundedSender<Self::Output>,
        state_reporter: ComponentStateReporter,
    ) -> anyhow::Result<()> {
        // Hand the reporter to FriJobManager — which is driven by HTTP handlers and add_job —
        // before any of those paths can fire. The manager's reporter() panics if unset.
        self.fri_job_manager.set_reporter(state_reporter);

        // State reporting for queued jobs is delegated to FriJobManager: FRI proving is
        // asynchronous (input → add_job → later proof on batches_with_proof_receiver), so
        // the manager calls record_processed when a proof is submitted. The passthrough
        // branch below records directly since those batches are already proved upstream.
        tokio::select! {
            result = async {
                while let Some(batch) = input.recv().await {
                    if batch.batch_number() > self.last_proved_batch_number {
                        tracing::info!(
                            "Received batch for FRI proving: {:?}",
                            batch.batch_number()
                        );
                        self.fri_job_manager.add_job(batch).await
                    } else {
                        let batch_with_fake_proof = batch.with_data(FriProof::AlreadySubmittedToL1);
                        if output
                            .send_and_record(batch_with_fake_proof, self.fri_job_manager.reporter())
                            .is_err()
                        {
                            return Ok::<(), anyhow::Error>(());
                        }
                    }
                }
                Ok::<(), anyhow::Error>(())
            } => {
                result?;
                tracing::info!("inbound channel closed");
                return Ok(());
            },
            _ = async {
                while let Some(proof) = self.batches_with_proof_receiver.recv().await {
                    tracing::info!(
                        "Received batch after FRI proving: {:?}",
                        proof.batch_number()
                    );
                    let _ = output.send(proof);
                }
            } => {
                tracing::info!("outbound channel closed");
                return Ok(());
            },
        }
    }
}
