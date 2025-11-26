use crate::prover_api::fri_job_manager::FriJob;
use crate::prover_api::metrics::{ProverStage, ProverType};
use crate::prover_api::prover_job_map::ProverJobMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::Sender;
use zksync_os_l1_sender::batcher_metrics::BatchExecutionStage;
use zksync_os_l1_sender::batcher_model::{
    FriProof, RealSnarkProof, SignedBatchEnvelope, SnarkProof,
};
use zksync_os_l1_sender::commands::prove::ProofCommand;
use zksync_os_observability::{
    ComponentStateHandle, ComponentStateReporter, GenericComponentState,
};
use zksync_os_types::ProvingVersion;

/// Job manager for SNARK proving.
///
/// Supports multiple SNARK provers
///
/// Supports both real and fake proofs.
///  - Fake FRI proofs always result in fake SNARK proofs.
///  - Real FRI proofs may result in real or fake SNARK proofs depending on prover availability
///
/// `SnarkJobManager` aims to assign real prover jobs to real SNARK provers -
///     but if jobs are not picked within a timeout (`max_batch_age`), it releases it to a fake prover
///
///
/// `ComponentStateLatencyTracker`: Only tracks `Processing` / `WaitingSend` states
pub struct SnarkJobManager {
    // == state ==
    jobs: ProverJobMap<FriProof>,
    // outbound
    prove_batches_sender: Sender<ProofCommand>,
    // config
    max_fris_per_snark: usize,
    // metrics
    latency_tracker: ComponentStateHandle<GenericComponentState>,
}

impl SnarkJobManager {
    pub fn new(
        // outbound
        prove_batches_sender: Sender<ProofCommand>,
        // config
        max_fris_per_snark: usize,
        assignment_timeout: Duration,
        max_assigned_batch_range: usize,
    ) -> Self {
        let jobs = ProverJobMap::<FriProof>::new(
            assignment_timeout,
            max_assigned_batch_range,
            ProverStage::Snark,
        );
        let latency_tracker = ComponentStateReporter::global().handle_for(
            "snark_job_manager",
            GenericComponentState::ProcessingOrWaitingRecv,
        );
        Self {
            jobs,
            prove_batches_sender,
            max_fris_per_snark,
            latency_tracker,
        }
    }

    /// Adds a pending job to the queue.
    /// Awaits if queue is full (ProverJobMap.max_assigned_batch_range).
    pub async fn add_job(&self, batch_envelope: SignedBatchEnvelope<FriProof>) {
        self.jobs.add_job(batch_envelope).await
    }

    // If there is a job pending, returns a non-empty list of tuples (`batch_number`, `verification_key_hash`, `real_fri_proof`)
    pub async fn pick_real_job(
        &self,
        prover_id: String,
    ) -> anyhow::Result<Option<Vec<(FriJob, FriProof)>>> {
        // consume/remove all fake jobs that may be in the front of the queue
        self.process_pending_fake_fri_proofs().await?;

        let batches_with_real_proofs = self
            .jobs
            .pick_jobs_while_with_limit(self.max_fris_per_snark, &prover_id, |job| {
                !job.batch_envelope.data.is_fake()
            })
            .await;

        if batches_with_real_proofs.is_empty() {
            tracing::trace!(prover_id, "no SNARK prove jobs are available for pick up",);
            return Ok(None);
        }

        Ok(Some(batches_with_real_proofs))
    }

    pub async fn submit_proof(
        &self,
        batch_from: u64,
        batch_to: u64,
        proving_version: Option<ProvingVersion>,
        payload: Vec<u8>,
        prover_id: String,
    ) -> anyhow::Result<()> {
        // note: we still hold mutex while verifying the proof -
        // this is desired since we don't want the batches to timeout

        // todo: verify_snark_proof()
        // if false {
        //     anyhow::bail!("proof validation failed")
        // }

        // prove is valid - consuming proven batches
        let Some(consumed_batches_proven) = self
            .jobs
            .complete_many_jobs(batch_from, batch_to, ProverType::Real, &prover_id)
            .await
        else {
            anyhow::bail!("race condition: some batches were completed earlier")
        };

        // Prover should generate the proof with VK received from server. These must always match.
        // If they don't, proof won't be accepted, validation will fail, therefore it's pointless to proceed.
        //
        // This should never happen, but we double-check to guarantee it's the case
        //
        // NOTE: Checking only if prover provided VK version - legacy clients may not provide it
        if let Some(proving_version) = proving_version {
            let server_vk = consumed_batches_proven[0]
                .batch
                .verification_key_hash()
                .expect("verification key hash must be present as it was set by server");
            let prover_vk = proving_version.vk_hash();
            anyhow::ensure!(
                server_vk == prover_vk,
                "Verification key hash mismatch: server got {server_vk}, prover got {prover_vk}"
            );
        }

        // get verification key, if available, otherwise fallback
        let proving_version = if let Some(proving_version) = proving_version {
            proving_version
        } else {
            consumed_batches_proven[0]
                .data
                .proving_execution_version()
                .unwrap_or(2)
                .try_into()
                .expect("execution version must exist as it was set by server")
        };

        let consumed_batches_proven: Vec<_> = consumed_batches_proven
            .into_iter()
            .map(|batch| batch.with_stage(BatchExecutionStage::SnarkProvedReal))
            .collect();

        self.send_downstream(ProofCommand::new(
            consumed_batches_proven,
            SnarkProof::Real(RealSnarkProof::V2 {
                proof: payload,
                proving_execution_version: proving_version as u32,
            }),
        ))
        .await?;
        Ok(())
    }

    /// Consumes fake FRI proofs from the head of the queue and turns them into fake SNARKs.
    async fn process_pending_fake_fri_proofs(&self) -> anyhow::Result<()> {
        self.process_pending_fake_or_timed_out_fri_proofs(None)
            .await
    }

    /// Consumes FRI proofs from the head of the queue that satisfy the following conditions:
    /// * FRI proof is fake
    /// * if `timeout_for_real_fris` is Some, then also jobs that are older than `timeout_for_real_fris`
    async fn process_pending_fake_or_timed_out_fri_proofs(
        &self,
        timeout_for_real_fris: Option<Duration>,
    ) -> anyhow::Result<()> {
        loop {
            let assigned: Vec<(FriJob, FriProof)> = self
                .jobs
                .pick_jobs_while_with_limit(self.max_fris_per_snark, "fake_prover", |job| {
                    job.batch_envelope.data.is_fake()
                        || (timeout_for_real_fris.is_some()
                            && job.metadata.added_at.elapsed() >= timeout_for_real_fris.unwrap())
                })
                .await;

            if assigned.is_empty() {
                return Ok(());
            }
            let real_proofs_count = assigned
                .iter()
                .filter(|(_, proof)| !proof.is_fake())
                .count();
            tracing::info!(
                "consuming fake proofs for SNARKing for batches {}-{} ({} real proofs; {} fake proofs)",
                assigned.first().unwrap().0.batch_number,
                assigned.last().unwrap().0.batch_number,
                real_proofs_count,
                assigned.len() - real_proofs_count,
            );

            let mut completed = Vec::default();
            for (job, _) in assigned {
                if let Some(envelope) = self
                    .jobs
                    .complete_job(job.batch_number, ProverType::Fake, "fake_prover")
                    .await
                {
                    completed.push(envelope);
                }
            }

            // Add observability traces
            let batches_with_fake_proofs = completed
                .into_iter()
                .map(|batch| batch.with_stage(BatchExecutionStage::SnarkProvedFake))
                .collect();

            self.send_downstream(ProofCommand::new(
                batches_with_fake_proofs,
                SnarkProof::Fake,
            ))
            .await?;
        }
    }

    async fn send_downstream(&self, proof_command: ProofCommand) -> anyhow::Result<()> {
        self.latency_tracker
            .enter_state(GenericComponentState::WaitingSend);
        self.prove_batches_sender.send(proof_command).await?;
        self.latency_tracker
            .enter_state(GenericComponentState::ProcessingOrWaitingRecv);
        Ok(())
    }
}

const POLL_INTERVAL_MS: u64 = 1000;

pub struct FakeSnarkProver {
    job_manager: Arc<SnarkJobManager>,

    // config
    max_batch_age: Duration,
    polling_interval: Duration,
}

impl FakeSnarkProver {
    pub fn new(job_manager: Arc<SnarkJobManager>, max_batch_age: Duration) -> Self {
        Self {
            job_manager,
            max_batch_age,
            polling_interval: Duration::from_millis(POLL_INTERVAL_MS),
        }
    }

    pub async fn run(self) -> anyhow::Result<()> {
        loop {
            tokio::time::sleep(self.polling_interval).await;
            self.job_manager
                .process_pending_fake_or_timed_out_fri_proofs(Some(self.max_batch_age))
                .await?;
        }
    }
}
