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
/// Supports multiple SNARK provers.
///
/// Supports both real and fake proofs.
///  - Fake FRI proofs always result in fake SNARK proofs.
///  - Real FRI proofs may result in real or fake SNARK proofs, depending on prover availability.
///
/// `SnarkJobManager` aims to assign real prover jobs to real SNARK provers. If
/// a job is not picked within a timeout (`max_batch_age`), it releases the job
/// to a fake prover.
pub struct SnarkJobManager {
    jobs: ProverJobMap<FriProof>,
    // outbound
    prove_batches_sender: mpsc::Sender<ProofCommand>,
    // config
    max_fris_per_snark: usize,
    /// Cut a range where the batches' semantic protocol version changes.
    ///
    /// Airbender only needs one proving version across a range, and two
    /// protocol versions can share it (0.31.0 and 0.31.1 are both V7). The
    /// second proof system is keyed more finely: its guest build, and so its
    /// verification keys, are pinned per protocol version, and it refuses to
    /// aggregate a range whose inputs mix key sets. A range straddling an
    /// upgrade would therefore be Airbender-valid and deterministically
    /// unaggregatable — a stall where the multi-proof is required. Set only
    /// when the second lane runs, so the Airbender-only shape is unchanged.
    cut_ranges_at_protocol_version: bool,
}

impl SnarkJobManager {
    pub fn new(
        prove_batches_sender: mpsc::Sender<ProofCommand>,
        max_fris_per_snark: usize,
        assignment_timeout: Duration,
        max_assigned_batch_range: usize,
        cut_ranges_at_protocol_version: bool,
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
            cut_ranges_at_protocol_version,
        }
    }

    pub async fn add_job(&self, batch_envelope: SignedBatchEnvelope<FriProof>) {
        self.jobs.add_job(batch_envelope).await
    }

    pub async fn pick_real_job(
        &self,
        prover_id: String,
        supported_proving_versions: Option<&[ProvingVersion]>,
    ) -> anyhow::Result<Option<Vec<(FriJob, FriProof)>>> {
        self.process_pending_fake_fri_proofs().await?;

        // The prover only receives batches whose proving version it declared
        // support for (`supported_proving_versions`). The aggregated ZiSK lane
        // pairs the assigned range with one aggregation range of the same
        // bounds. A range of one batch is valid — the aggregator guest and the
        // L1 aggregation verifier both accept any width — so the pick forms
        // whatever consecutive group is ready. There is no fixed-size group and
        // no stranded wind-down tail.
        // Collected as the pick runs: `FriJob` carries only the batch number
        // and VK hash, and the range has to be cut on the batch's semantic
        // protocol version.
        let mut versions = std::collections::HashMap::new();
        let mut batches_with_real_proofs = self
            .jobs
            .pick_jobs_while_with_limit(self.max_fris_per_snark, &prover_id, |job| {
                let eligible = !job.batch_envelope.data.is_fake()
                    && supported_proving_versions
                        .is_none_or(|versions| versions.contains(&job.metadata.proving_version));
                if eligible {
                    versions.insert(
                        job.metadata.batch_number,
                        job.batch_envelope.batch.batch_info.protocol_version.clone(),
                    );
                }
                eligible
            })
            .await;

        if self.cut_ranges_at_protocol_version {
            let first = batches_with_real_proofs
                .first()
                .and_then(|(job, _)| versions.get(&job.batch_number));
            let cut = batches_with_real_proofs
                .iter()
                .position(|(job, _)| versions.get(&job.batch_number) != first);
            if let Some(cut) = cut {
                let dropped: Vec<u64> = batches_with_real_proofs
                    .drain(cut..)
                    .map(|(job, _)| job.batch_number)
                    .collect();
                for batch_number in &dropped {
                    self.jobs.unassign_job(*batch_number, &prover_id).await;
                }
                tracing::info!(
                    prover_id,
                    from = dropped.first(),
                    "cut the SNARK range at a protocol version boundary so the second proof \
                     system can aggregate it"
                );
            }
        }

        if batches_with_real_proofs.is_empty() {
            tracing::trace!(prover_id, "no SNARK prove jobs are available for pick up");
            return Ok(None);
        }

        Ok(Some(batches_with_real_proofs))
    }

    /// Submit a real Airbender SNARK proof.
    ///
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

        // Prover should generate the proof with VK received from server. These must always match.
        // If they don't, proof won't be accepted, validation will fail, therefore it's pointless to proceed.
        //
        // This should never happen, but we double-check to guarantee it's the case.
        let Some(batch_metadata) = self.jobs.get_job_batch_metadata(batch_from).await else {
            anyhow::bail!("race condition: some batches were completed earlier")
        };
        let server_vk = batch_metadata
            .verification_key_hash()
            .expect("verification key hash must be present as it was set by server");
        let prover_vk = proving_version.vk_hash();
        anyhow::ensure!(
            server_vk == prover_vk,
            "Verification key hash mismatch: server got {server_vk}, prover got {prover_vk}"
        );

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

        permit.send(ProofCommand::new(
            consumed_batches_proven,
            SnarkProof::Real(RealSnarkProof::V2 {
                proof: payload,
                proving_execution_version: proving_version as u32,
            }),
        ));
        Ok(())
    }

    async fn process_pending_fake_fri_proofs(&self) -> anyhow::Result<()> {
        self.process_pending_fake_or_timed_out_fri_proofs(None)
            .await
    }

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
            if !self.jobs.has_assignable_job(is_fake_or_timed_out).await {
                return Ok(());
            }

            let permit = self.try_reserve_permit_downstream()?;
            let assigned: Vec<(FriJob, FriProof)> = self
                .jobs
                .pick_jobs_while_with_limit(
                    self.max_fris_per_snark,
                    "fake_prover",
                    is_fake_or_timed_out,
                )
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
            let batches_with_fake_proofs = completed
                .into_iter()
                .map(|batch| batch.with_stage(BatchExecutionStage::SnarkProvedFake))
                .collect();

            permit.send(ProofCommand::new(
                batches_with_fake_proofs,
                SnarkProof::Fake,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prover_api::test_util::create_test_batch_envelope_with_data;
    use zksync_os_types::ProtocolSemanticVersion;

    #[tokio::test]
    async fn backpressure_does_not_lease_fake_jobs() {
        let protocol_version = ProtocolSemanticVersion::new(0, 32, 0);
        let (sender, mut receiver) = mpsc::channel(1);
        sender
            .try_send(ProofCommand::new(
                vec![create_test_batch_envelope_with_data(
                    100,
                    protocol_version.clone(),
                    FriProof::Fake,
                )],
                SnarkProof::Fake,
            ))
            .unwrap();

        let manager = SnarkJobManager::new(sender, 1, Duration::from_secs(60), 100, false);
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

    /// Airbender is happy to prove a range that straddles a protocol upgrade —
    /// 0.31.0 and 0.31.1 are both proving version V7. The second proof system
    /// pins its guest build per protocol version and refuses to aggregate a
    /// range that mixes them, so such a range would be deterministically
    /// unaggregatable. Cut it at the boundary instead, and leave the tail
    /// pickable so the next range starts there.
    #[tokio::test]
    async fn range_is_cut_at_a_protocol_version_boundary() {
        use zksync_os_batch_types::batcher_model::RealFriProof;
        use zksync_os_types::ProtocolSemanticVersion;

        let (sender, _receiver) = mpsc::channel(4);
        let manager = SnarkJobManager::new(sender, 8, Duration::from_secs(300), 100, true);

        let real = || FriProof::Real(RealFriProof::V1(vec![0u8; 8].into()));
        for batch in 1..=4u64 {
            // Batches 3 and 4 land after a patch upgrade.
            let protocol_version = if batch <= 2 {
                ProtocolSemanticVersion::new(0, 31, 0)
            } else {
                ProtocolSemanticVersion::new(0, 31, 1)
            };
            manager
                .add_job(create_test_batch_envelope_with_data(
                    batch,
                    protocol_version,
                    real(),
                ))
                .await;
        }

        let picked = manager
            .pick_real_job("prover-1".to_string(), None)
            .await
            .expect("pick succeeds")
            .expect("a range is available");
        assert_eq!(
            picked
                .iter()
                .map(|(job, _)| job.batch_number)
                .collect::<Vec<_>>(),
            vec![1, 2],
            "the range stops at the version boundary"
        );

        // The tail was not consumed: another prover picks it as its own range.
        let next = manager
            .pick_real_job("prover-2".to_string(), None)
            .await
            .expect("pick succeeds")
            .expect("the tail is still pickable");
        assert_eq!(
            next.iter()
                .map(|(job, _)| job.batch_number)
                .collect::<Vec<_>>(),
            vec![3, 4],
            "the cut tail forms the next range"
        );
    }
}
