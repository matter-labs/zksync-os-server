use crate::prover_api::fri_job_manager::FriJob;
use crate::prover_api::metrics::{ProverStage, ProverType};
use crate::prover_api::prover_job_manager_state::ProverJobManagerState;
use crate::prover_api::prover_job_map::ProverJobMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;
use zksync_os_l1_sender::batcher_metrics::BatchExecutionStage;
use zksync_os_l1_sender::batcher_model::{
    FriProof, RealSnarkProof, SignedBatchEnvelope, SnarkProof,
};
use zksync_os_l1_sender::commands::prove::ProofCommand;
use zksync_os_observability::ComponentStateReporter;
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
/// State model: see `ProverJobManagerState`. `Idle` when queue is empty,
/// `WaitingForProver` while jobs are out with external provers (the steady
/// state under normal load), `ProcessingSubmission` during brief
/// add_job/submit/send windows.
pub struct SnarkJobManager {
    // == state ==
    jobs: ProverJobMap<FriProof>,
    // outbound
    prove_batches_sender: UnboundedSender<ProofCommand>,
    // config
    max_fris_per_snark: usize,
    // metrics
    state_reporter: OnceLock<ComponentStateReporter>,
}

impl SnarkJobManager {
    pub fn new(
        prove_batches_sender: UnboundedSender<ProofCommand>,
        max_fris_per_snark: usize,
        assignment_timeout: Duration,
        max_assigned_batch_range: usize,
    ) -> Self {
        let jobs = ProverJobMap::<FriProof>::new(
            assignment_timeout,
            max_assigned_batch_range,
            ProverStage::Snark,
        );
        Self {
            jobs,
            prove_batches_sender,
            max_fris_per_snark,
            state_reporter: OnceLock::new(),
        }
    }

    /// Late-install the reporter. Called once from `SnarkProvingPipelineStep::run()` after
    /// `Pipeline::pipe()` has auto-registered the component with the monitor.
    pub fn set_reporter(&self, reporter: ComponentStateReporter) {
        reporter.enter_state(ProverJobManagerState::Idle);
        self.state_reporter
            .set(reporter)
            .expect("SnarkJobManager::set_reporter called more than once");
    }

    pub(crate) fn reporter(&self) -> &ComponentStateReporter {
        self.state_reporter.get().expect(
            "SnarkJobManager reporter not initialized — set_reporter must be called \
             from SnarkProvingPipelineStep::run() before any record_* path",
        )
    }

    /// Adds a pending job to the queue.
    /// Awaits if queue is full (ProverJobMap.max_assigned_batch_range).
    pub async fn add_job(&self, batch_envelope: SignedBatchEnvelope<FriProof>) {
        // Capture coordinates before moving into jobs
        let last_block = batch_envelope.batch.last_block_number;
        let timestamp = Some(batch_envelope.batch.batch_info.last_block_timestamp);
        let batch_number = batch_envelope.batch_number();

        self.reporter()
            .enter_state(ProverJobManagerState::ProcessingSubmission);
        tracing::debug!(
            batch_number,
            "SnarkJobManager: queuing SNARK job for batch {batch_number}, last_block={last_block}"
        );
        self.jobs.add_job(batch_envelope).await;
        tracing::debug!(
            batch_number,
            "SnarkJobManager: SNARK job {batch_number} accepted into queue, last_block={last_block}"
        );

        self.reporter()
            .record_picked(last_block, timestamp, Some(batch_number));
        self.update_in_flight_state().await;
    }

    async fn update_in_flight_state(&self) {
        let range = self.jobs.in_flight_range().await;
        let (first, last) = match range {
            Some((f, l)) => {
                tracing::debug!(
                    "SnarkJobManager: in-flight range updated: batches {}-{}, blocks {}-{}",
                    f.batch_number,
                    l.batch_number,
                    f.last_block_number,
                    l.last_block_number,
                );
                self.reporter()
                    .enter_state(ProverJobManagerState::WaitingForProver);
                (Some(f), Some(l))
            }
            None => {
                tracing::debug!("SnarkJobManager: in-flight range cleared (queue empty)");
                self.reporter().enter_state(ProverJobManagerState::Idle);
                (None, None)
            }
        };
        self.reporter().record_in_flight_range(first, last);
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
            tracing::trace!("no SNARK prove jobs are available for pick up, prover={prover_id}");
            return Ok(None);
        }

        Ok(Some(batches_with_real_proofs))
    }

    pub async fn submit_proof(
        &self,
        batch_from: u64,
        batch_to: u64,
        proving_version: ProvingVersion,
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
        // This should never happen, but we double-check to guarantee it's the case.
        let server_vk = consumed_batches_proven[0]
            .batch
            .verification_key_hash()
            .expect("verification key hash must be present as it was set by server");
        let prover_vk = proving_version.vk_hash();
        anyhow::ensure!(
            server_vk == prover_vk,
            "Verification key hash mismatch: server got {server_vk}, prover got {prover_vk}"
        );

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
            tracing::debug!(
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
        // Use last_block_number (not batch_number): the monitor lag computation is block-based.
        let last = proof_command.as_ref().last().unwrap();
        let seq = last.batch.last_block_number;
        let last_block_timestamp = last.batch.batch_info.last_block_timestamp;
        let batch_number = last.batch_number();
        let batch_count = proof_command.as_ref().len();
        self.reporter()
            .enter_state(ProverJobManagerState::ProcessingSubmission);
        tracing::debug!(
            batch_number,
            "SnarkJobManager: sending SNARK proof downstream for batch {batch_number}, batch_count={batch_count}, last_block={seq}"
        );
        self.prove_batches_sender.send(proof_command)?;
        self.reporter()
            .record_processed(seq, Some(last_block_timestamp), Some(batch_number));
        self.update_in_flight_state().await;
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

    pub async fn run(self) {
        loop {
            tokio::time::sleep(self.polling_interval).await;
            self.job_manager
                .process_pending_fake_or_timed_out_fri_proofs(Some(self.max_batch_age))
                .await
                .expect("snark prover failed");
        }
    }
}
