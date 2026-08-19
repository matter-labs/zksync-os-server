use crate::prover_api::fri_job_manager::FriJob;
use crate::prover_api::metrics::{ProverStage, ProverType};
use crate::prover_api::prover_job_map::{JobEntry, ProverJobMap};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::sync::mpsc::Permit;
use tokio::sync::mpsc::error::TrySendError;
use zksync_os_batch_types::batcher_model::{
    FriProof, RealSnarkProof, SignedBatchEnvelope, SnarkProof,
};
use zksync_os_batcher_metrics::BatchExecutionStage;
use zksync_os_l1_sender::commands::prove::ProofCommand;
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
pub struct SnarkJobManager {
    // == state ==
    jobs: ProverJobMap<FriProof>,
    // outbound
    prove_batches_sender: mpsc::Sender<ProofCommand>,
    // config
    max_fris_per_snark: usize,
}

impl SnarkJobManager {
    pub fn new(
        prove_batches_sender: mpsc::Sender<ProofCommand>,
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
        }
    }

    pub async fn add_job(&self, batch_envelope: SignedBatchEnvelope<FriProof>) {
        self.jobs.add_job(batch_envelope).await
    }

    // If there is a job pending, returns a non-empty list of tuples (`batch_number`, `verification_key_hash`, `real_fri_proof`)
    pub async fn pick_real_job(
        &self,
        prover_id: String,
        supported_proving_versions: Option<&[ProvingVersion]>,
    ) -> anyhow::Result<Option<Vec<(FriJob, FriProof)>>> {
        // consume/remove all fake jobs that may be in the front of the queue
        self.process_pending_fake_fri_proofs().await?;

        // Same version-boundary split as the fake path. The predicate also runs on head jobs
        // that end up skipped, so the version must be pinned only after the other checks pass —
        // pinning from a rejected job would block every later job of a different version.
        let mut group_version = None;
        let batches_with_real_proofs = self
            .jobs
            .pick_jobs_while_with_limit(self.max_fris_per_snark, &prover_id, |job| {
                if job.batch_envelope.data.is_fake()
                    || !supported_proving_versions
                        .is_none_or(|versions| versions.contains(&job.metadata.proving_version))
                {
                    return false;
                }
                let version = &job.batch_envelope.batch.batch_info.protocol_version;
                *group_version.get_or_insert_with(|| version.clone()) == *version
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

        // The range is prover-echoed, so re-validate what the pickers guarantee at hand-out:
        // every batch was proven with the server's VK, and the range does not span a
        // protocol-version boundary (the verifying executor applies one PI formula per SNARK).
        // Per-batch metadata is immutable, so checking before consumption is race-safe —
        // `complete_many_jobs` re-checks existence under its own lock.
        let mut group_version = None;
        for batch_number in batch_from..=batch_to {
            let Some(batch_metadata) = self.jobs.get_job_batch_metadata(batch_number).await else {
                anyhow::bail!("race condition: some batches were completed earlier")
            };
            let server_vk = batch_metadata
                .verification_key_hash()
                .expect("verification key hash must be present as it was set by server");
            let prover_vk = proving_version.vk_hash();
            anyhow::ensure!(
                server_vk == prover_vk,
                "Verification key hash mismatch for batch {batch_number}: server got {server_vk}, prover got {prover_vk}"
            );
            let version = &batch_metadata.batch_info.protocol_version;
            match &group_version {
                None => group_version = Some(version.clone()),
                Some(first) => anyhow::ensure!(
                    first == version,
                    "batch range {batch_from}..={batch_to} spans a protocol-version boundary \
                     ({first:?} vs {version:?} at batch {batch_number}); one SNARK must cover one version"
                ),
            }
        }

        // Ensure we can send downstream before consuming jobs from the retryable map.
        let permit = self.try_reserve_permit_downstream()?;

        // prove is valid - consuming proven batches
        let Some(consumed_batches_proven) = self
            .jobs
            .complete_many_jobs(batch_from, batch_to, ProverType::Real, &prover_id)
            .await
        else {
            anyhow::bail!("race condition: some batches were completed earlier")
        };

        let consumed_batches_proven: Vec<_> = consumed_batches_proven
            .into_iter()
            .map(|batch| batch.with_stage(BatchExecutionStage::SnarkProvedReal))
            .collect();

        let chain_config_hash = proof_chain_config_hash(&consumed_batches_proven)?;
        permit.send(ProofCommand::new(
            consumed_batches_proven,
            SnarkProof::Real(RealSnarkProof::V2 {
                proof: payload,
                proving_execution_version: proving_version as u32,
            }),
            chain_config_hash,
        ));
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
            let is_fake_or_timed_out = |job: &JobEntry<FriProof>| {
                job.batch_envelope.data.is_fake()
                    || timeout_for_real_fris
                        .is_some_and(|timeout| job.metadata.added_at.elapsed() >= timeout)
            };
            if !self.jobs.has_assignable_job(&is_fake_or_timed_out).await {
                return Ok(());
            }

            let permit = self.try_reserve_permit_downstream()?;
            // A SNARK must not span a protocol-version boundary: the verifying executor
            // applies its own PI formula to every batch in the group. The version is pinned
            // only after the fake/timeout check passes — the predicate also runs on head jobs
            // that end up skipped, and pinning from one would block later versions.
            let mut group_version = None;
            let assigned: Vec<(FriJob, FriProof)> = self
                .jobs
                .pick_jobs_while_with_limit(self.max_fris_per_snark, "fake_prover", |job| {
                    if !is_fake_or_timed_out(job) {
                        return false;
                    }
                    let version = &job.batch_envelope.batch.batch_info.protocol_version;
                    *group_version.get_or_insert_with(|| version.clone()) == *version
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

            let batch_from = assigned.first().unwrap().0.batch_number;
            let batch_to = assigned.last().unwrap().0.batch_number;
            let Some(completed) = self
                .jobs
                .complete_many_jobs(batch_from, batch_to, ProverType::Fake, "fake_prover")
                .await
            else {
                tracing::info!(
                    batch_from,
                    batch_to,
                    "skipping fake SNARK proof because another prover completed part of the range"
                );
                continue;
            };

            // Add observability traces
            let batches_with_fake_proofs: Vec<_> = completed
                .into_iter()
                .map(|batch| batch.with_stage(BatchExecutionStage::SnarkProvedFake))
                .collect();

            let chain_config_hash = proof_chain_config_hash(&batches_with_fake_proofs)?;
            permit.send(ProofCommand::new(
                batches_with_fake_proofs,
                SnarkProof::Fake,
                chain_config_hash,
            ));
        }
    }

    fn try_reserve_permit_downstream(&self) -> anyhow::Result<Permit<'_, ProofCommand>> {
        Ok(match self.prove_batches_sender.try_reserve() {
            Ok(permit) => permit,
            Err(TrySendError::Full(_)) => {
                anyhow::bail!("downstream backpressure");
            }
            Err(TrySendError::Closed(_)) => {
                anyhow::bail!("server is shutting down");
            }
        })
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
            if let Err(err) = self
                .job_manager
                .process_pending_fake_or_timed_out_fri_proofs(Some(self.max_batch_age))
                .await
            {
                tracing::info!("`FakeSnarkProver` iteration failed: {err}");
            }
        }
    }
}

/// Chain-config-hash word of the batch proof public input: `Some` for v32+, `None` before.
/// Groups never mix protocol versions (enforced at pick-time and re-validated for
/// prover-echoed ranges in `submit_proof`), so the last batch's version is the group's.
fn proof_chain_config_hash(
    batches: &[zksync_os_batch_types::batcher_model::SignedBatchEnvelope<
        zksync_os_batch_types::batcher_model::FriProof,
    >],
) -> anyhow::Result<Option<alloy::primitives::B256>> {
    let batch_info = &batches
        .last()
        .expect("proof command must contain at least one batch")
        .batch
        .batch_info;
    if batch_info.protocol_version.supports_l1_interop() {
        Ok(Some(zksync_os_native_pig::v32_chain_config_hash(
            batch_info.chain_id,
        )?))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prover_api::test_util::create_test_batch_envelope_with_data;
    use zksync_os_types::ProtocolSemanticVersion;

    #[tokio::test]
    async fn backpressure_does_not_lease_fake_jobs() {
        let protocol_version = ProtocolSemanticVersion::new(0, 32, 0);
        let (sender, mut receiver) = mpsc::channel(1);
        let capacity_filler = vec![create_test_batch_envelope_with_data(
            100,
            protocol_version.clone(),
            FriProof::Fake,
        )];
        let chain_config_hash = proof_chain_config_hash(&capacity_filler).unwrap();
        sender
            .try_send(ProofCommand::new(
                capacity_filler,
                SnarkProof::Fake,
                chain_config_hash,
            ))
            .unwrap();

        let manager = SnarkJobManager::new(sender, 1, Duration::from_secs(60), 100);
        manager
            .add_job(create_test_batch_envelope_with_data(
                1,
                protocol_version,
                FriProof::Fake,
            ))
            .await;

        let err = manager.process_pending_fake_fri_proofs().await.unwrap_err();
        assert_eq!(err.to_string(), "downstream backpressure");
        let status = manager.jobs.status().await;
        assert_eq!(status[0].assigned_to_prover_id, None);
        assert_eq!(status[0].current_attempt, 0);

        receiver.recv().await.unwrap();
        manager.process_pending_fake_fri_proofs().await.unwrap();

        let command = receiver.recv().await.unwrap();
        assert_eq!(command.as_ref()[0].batch_number(), 1);
        assert!(manager.jobs.status().await.is_empty());
    }
}
