use crate::prover_api::fri_job_manager::FriJob;
use crate::prover_api::metrics::{ProverStage, ProverType};
use crate::prover_api::prover_job_map::ProverJobMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::sync::mpsc::Permit;
use tokio::sync::mpsc::error::TrySendError;
use zisk_prover_lane::ZiskAggregationJobManager;
use zisk_prover_lane::ZiskJobManager;
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
///
/// When the second proof system is configured, `submit_proof` enters the ZiSK
/// multi-proof combination. See [`super::multiproof_combine`] for that path.
pub struct SnarkJobManager {
    // `pub(super)` fields and methods below are reached from the sibling
    // `multiproof_combine` module, which extends this manager with the ZiSK path.
    pub(super) jobs: ProverJobMap<FriProof>,
    // outbound
    prove_batches_sender: mpsc::Sender<ProofCommand>,
    // config
    max_fris_per_snark: usize,
    pub(super) zisk_job_manager: Option<Arc<ZiskJobManager>>,
    /// When set, the ZiSK lane runs in AGGREGATED mode: the combination
    /// pairs the Airbender range SNARK with one aggregated ZiSK range
    /// proof instead of a per-batch PLONK proof.
    pub(super) zisk_aggregation_job_manager: Option<Arc<ZiskAggregationJobManager>>,
    /// When true, refuse to send Airbender-only proofs if ZiSK data was expected.
    /// Prevents silent fallback to single-proof mode when ZiSK provers are offline.
    pub(super) require_multi_proof: bool,
    /// How long a batch may block on its ZiSK proof path before an
    /// Airbender-only submission is allowed despite `require_multi_proof`.
    /// `None`: block indefinitely (the operator escape hatch is flipping the
    /// config — no deploy needed). Measured from when the batch entered SNARK
    /// proving.
    pub(super) multi_proof_wait_timeout: Option<Duration>,
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
            zisk_job_manager: None,
            zisk_aggregation_job_manager: None,
            require_multi_proof: false,
            multi_proof_wait_timeout: None,
        }
    }

    /// See [`Self::multi_proof_wait_timeout`].
    pub fn set_multi_proof_wait_timeout(&mut self, timeout: Option<Duration>) {
        self.multi_proof_wait_timeout = timeout;
    }

    /// When true, batches with ZiSK data will NOT fall back to Airbender-only.
    /// They will be held until the ZiSK prover processes them.
    pub fn set_require_multi_proof(&mut self, require: bool) {
        self.require_multi_proof = require;
    }

    /// Set the ZiSK job manager for routing Airbender SNARKs to multi-proof composition.
    pub fn set_zisk_job_manager(&mut self, zisk_job_manager: Arc<ZiskJobManager>) {
        self.zisk_job_manager = Some(zisk_job_manager);
    }

    /// Switch the combination to aggregated mode (see the struct docs).
    pub fn set_zisk_aggregation_job_manager(
        &mut self,
        zisk_aggregation_job_manager: Arc<ZiskAggregationJobManager>,
    ) {
        self.zisk_aggregation_job_manager = Some(zisk_aggregation_job_manager);
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
        let batches_with_real_proofs = self
            .jobs
            .pick_jobs_while_with_limit(self.max_fris_per_snark, &prover_id, |job| {
                !job.batch_envelope.data.is_fake()
                    && supported_proving_versions
                        .is_none_or(|versions| versions.contains(&job.metadata.proving_version))
            })
            .await;

        if batches_with_real_proofs.is_empty() {
            tracing::trace!(prover_id, "no SNARK prove jobs are available for pick up");
            return Ok(None);
        }

        // Aggregated mode: the assigned range IS the ZiSK aggregation
        // range. Register it now so the aggregation proof is computed
        // while the Airbender SNARK is still being proven; the submission
        // re-registers authoritatively (a timed-out range may be re-picked
        // with different bounds).
        if let (Some(zisk_aggregation_job_manager), Some((first, _)), Some((last, _))) = (
            self.zisk_aggregation_job_manager.as_ref(),
            batches_with_real_proofs.first(),
            batches_with_real_proofs.last(),
        ) {
            zisk_aggregation_job_manager
                .note_snark_range(first.batch_number, last.batch_number)
                .await;
        }

        Ok(Some(batches_with_real_proofs))
    }

    /// Submit a real Airbender SNARK proof.
    ///
    /// When the ZiSK lane is configured, the submission enters the aggregated
    /// multi-proof combination (see [`super::multiproof_combine`]): the
    /// Airbender range SNARK pairs with the aggregated ZiSK range proof of the
    /// same bounds. Otherwise it is the upstream Airbender-only path.
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

        // Multi-proof combination: only when the second proof-system lane is
        // configured. When it is not, the submission below is the upstream
        // Airbender-only path verbatim.
        if self.zisk_job_manager.is_some() {
            return self
                .submit_with_multiproof_combine(
                    batch_from,
                    batch_to,
                    proving_version,
                    payload,
                    prover_id,
                )
                .await;
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

        permit.send(ProofCommand::new(
            consumed_batches_proven,
            SnarkProof::Real(RealSnarkProof::V2 {
                proof: payload,
                proving_execution_version: proving_version as u32,
            }),
        ));
        Ok(())
    }

    /// Send an Airbender-only SNARK proof downstream via a reserved permit.
    /// The multi-proof gate lives in `submit_proof` BEFORE job consumption;
    /// by the time this runs the submission has already been allowed through.
    pub(super) fn send_airbender_only(
        &self,
        permit: Permit<'_, ProofCommand>,
        batches: Vec<SignedBatchEnvelope<FriProof>>,
        payload: Vec<u8>,
        proving_version: ProvingVersion,
    ) -> anyhow::Result<()> {
        permit.send(ProofCommand::new(
            batches,
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

            let batch_from = assigned.first().unwrap().0.batch_number;
            let batch_to = assigned.last().unwrap().0.batch_number;
            let permit = self.try_reserve_permit_downstream()?;
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

            // Fake SNARKs can never be composed into a MultiProof — drop the
            // batches' ZiSK lane state (jobs created at seal, parked proofs) so
            // fake-prover environments don't accumulate orphans. Inert when the
            // second proof system is off (no ZiSK job manager).
            if let Some(zisk_job_manager) = self.zisk_job_manager.as_ref() {
                zisk_job_manager.discard_batches(batch_from, batch_to).await;
            }

            permit.send(ProofCommand::new(
                batches_with_fake_proofs,
                SnarkProof::Fake,
            ));
        }
    }

    pub(super) fn try_reserve_permit_downstream(&self) -> anyhow::Result<Permit<'_, ProofCommand>> {
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
    use crate::batcher::zisk_batch::ZiskChainConfig;
    use crate::prover_api::test_util::create_test_batch_envelope;
    use crate::prover_api::zisk_proof_constants::{
        ZISK_PUBLIC_VALUES_BYTES, ZISK_SNARK_PROOF_BYTES,
    };
    use zisk_prover_lane::ZiskJobData;
    use zksync_os_types::ProtocolSemanticVersion;

    const TEST_CHAIN_ID: u64 = 270;
    const TEST_CHAIN_CONFIG: ZiskChainConfig = ZiskChainConfig {
        fri_proof_verification_enabled: false,
        max_tx_gas_limit: 1 << 24,
    };

    fn envelope(batch: u64) -> SignedBatchEnvelope<FriProof> {
        let mut e = create_test_batch_envelope(batch, FriProof::Fake);
        e.batch.batch_info.protocol_version = ProtocolSemanticVersion::new(0, 31, 0);
        e
    }

    // ---- aggregated mode ----

    use alloy::primitives::B256;
    use zisk_prover_lane::synthetic_stream;
    use zisk_prover_lane::{
        AggregationInput, ZiskAggregationJobManager, expected_aggregated_public_input,
    };

    const TEST_PROGRAM_VK: [u64; 4] = [1, 2, 3, 4];
    const TEST_VADCOP_VK: [u64; 4] = [5, 6, 7, 8];

    fn vk_be(limbs: [u64; 4]) -> B256 {
        let mut out = [0u8; 32];
        for (i, chunk) in out.chunks_exact_mut(8).enumerate() {
            chunk.copy_from_slice(&limbs[i].to_be_bytes());
        }
        B256::from(out)
    }

    /// A SNARK manager wired for AGGREGATED mode over 2-batch ranges, with
    /// jobs for batches 1..=2 added.
    async fn aggregated_manager(
        require: bool,
        wait_timeout: Option<Duration>,
    ) -> (
        SnarkJobManager,
        Arc<ZiskJobManager>,
        Arc<ZiskAggregationJobManager>,
        mpsc::Receiver<ProofCommand>,
        ProvingVersion,
    ) {
        let (tx, rx) = mpsc::channel(4);
        let mut snark_job_manager = SnarkJobManager::new(tx, 2, Duration::from_secs(60), 10);
        let zisk_job_manager = Arc::new(ZiskJobManager::new(
            Duration::from_secs(60),
            None,
            None,
            TEST_CHAIN_ID,
            TEST_CHAIN_CONFIG,
            // These tests submit synthetic ZiSK proofs, so proof verification
            // is disabled; the batch commitment binding still runs.
            false,
        ));
        let zisk_aggregation_job_manager = Arc::new(ZiskAggregationJobManager::new(
            2,
            Duration::from_secs(60),
            None,
            None,
            false,
        ));
        zisk_job_manager.set_aggregation_sink(zisk_aggregation_job_manager.clone());
        snark_job_manager.set_zisk_job_manager(zisk_job_manager.clone());
        snark_job_manager.set_zisk_aggregation_job_manager(zisk_aggregation_job_manager.clone());
        snark_job_manager.set_require_multi_proof(require);
        snark_job_manager.set_multi_proof_wait_timeout(wait_timeout);
        let mut proving_version = None;
        for batch in 1..=2 {
            let envelope = envelope(batch);
            proving_version = Some(envelope.batch.proving_version().expect("proving version"));
            snark_job_manager.add_job(envelope).await;
        }
        (
            snark_job_manager,
            zisk_job_manager,
            zisk_aggregation_job_manager,
            rx,
            proving_version.unwrap(),
        )
    }

    /// Run a batch's per-batch ZiSK job through the manager in aggregated
    /// mode (matching vadcop_final stream), returning the batch commitment.
    async fn submit_zisk_stream(zisk_job_manager: &ZiskJobManager, batch: u64) -> B256 {
        let batch_metadata = envelope(batch).batch;
        let stored = batch_metadata.batch_info.clone().into_stored();
        let prev = &batch_metadata.previous_stored_batch_info;
        let commitment = zisk_prover_lane::expected_zisk_public_input(
            &prev.state_commitment,
            &stored,
            TEST_CHAIN_ID,
            TEST_CHAIN_CONFIG,
        );
        let stream = synthetic_stream(TEST_PROGRAM_VK, TEST_VADCOP_VK, commitment.0);
        zisk_job_manager
            .add_job(
                batch,
                ZiskJobData {
                    zisk_data: vec![0xAB; 16],
                    batch_metadata,
                    added_at: std::time::Instant::now(),
                },
            )
            .await;
        zisk_job_manager
            .pick_next_job("zisk-prover")
            .await
            .expect("job available");
        zisk_job_manager
            .submit_proof(batch, stream, vec![], "zisk-prover")
            .await
            .expect("zisk stream accepted");
        commitment
    }

    /// Aggregated public values whose digest matches the given commitments.
    fn aggregated_public_values(commitments: &[B256]) -> Vec<u8> {
        let inputs: Vec<AggregationInput> = commitments
            .iter()
            .map(|&commitment| AggregationInput {
                stream: vec![],
                program_vk: vk_be(TEST_PROGRAM_VK),
                vadcop_vk: vk_be(TEST_VADCOP_VK),
                commitment,
            })
            .collect();
        let refs: Vec<&AggregationInput> = inputs.iter().collect();
        let digest = expected_aggregated_public_input(&refs).expect("digest");
        let mut pv = vec![0u8; ZISK_PUBLIC_VALUES_BYTES];
        pv[32..64].copy_from_slice(digest.as_slice());
        pv
    }

    /// The aggregated combination end to end: a blocked Airbender range
    /// submission registers the range; the per-batch streams arrive; the
    /// aggregation prover proves the range; the re-submitted Airbender
    /// range SNARK composes the range MultiProof carrying the AGGREGATED
    /// proof, and the per-batch markers are swept.
    #[tokio::test]
    async fn aggregated_combination_composes_range_multi_proof() {
        let (
            snark_job_manager,
            zisk_job_manager,
            zisk_aggregation_job_manager,
            mut rx,
            proving_version,
        ) = aggregated_manager(true, None).await;

        // Airbender arrives first: blocked, range registered, jobs kept.
        let err = snark_job_manager
            .submit_proof(1, 2, proving_version, vec![0xAA; 8], "prover-1".into())
            .await
            .expect_err("must block until the aggregated proof lands");
        assert!(err.to_string().contains("aggregated ZiSK proof"), "{err}");
        assert!(
            snark_job_manager
                .jobs
                .get_job_batch_metadata(1)
                .await
                .is_some()
        );

        // Per-batch streams land; the range forms and gets proven.
        let c1 = submit_zisk_stream(&zisk_job_manager, 1).await;
        let c2 = submit_zisk_stream(&zisk_job_manager, 2).await;
        let job = zisk_aggregation_job_manager
            .pick_next_job("agg-1")
            .await
            .expect("aggregation job");
        assert_eq!((job.from_batch, job.to_batch), (1, 2));
        zisk_aggregation_job_manager
            .submit_proof(
                1,
                2,
                vec![0x77; ZISK_SNARK_PROOF_BYTES],
                aggregated_public_values(&[c1, c2]),
                "agg-1",
            )
            .await
            .expect("aggregated proof accepted");

        // The Airbender range SNARK now composes the range MultiProof.
        snark_job_manager
            .submit_proof(1, 2, proving_version, vec![0xAA; 8], "prover-1".into())
            .await
            .expect("combination must compose");
        let cmd = rx.try_recv().expect("MultiProof command sent downstream");
        let (batches, proof) = cmd.into_parts();
        assert_eq!(batches.len(), 2, "the command covers the whole range");
        match proof {
            SnarkProof::MultiProof(mp) => {
                assert_eq!(mp.airbender_proof, vec![0xAA; 8]);
                assert_eq!(mp.zisk_proof, vec![0x77; ZISK_SNARK_PROOF_BYTES]);
            }
            other => panic!("expected MultiProof, got {other:?}"),
        }
        // Consumed range: markers swept, aggregated proof taken exactly once.
        assert!(
            zisk_aggregation_job_manager
                .take_completed(1, 2)
                .await
                .is_none()
        );
    }

    /// Aggregated + optional (shadow) mode: nothing blocks, the range goes
    /// downstream Airbender-only, and the aggregation state for the
    /// consumed batches is swept.
    #[tokio::test]
    async fn aggregated_optional_mode_sends_airbender_only() {
        let (
            snark_job_manager,
            zisk_job_manager,
            zisk_aggregation_job_manager,
            mut rx,
            proving_version,
        ) = aggregated_manager(false, None).await;
        submit_zisk_stream(&zisk_job_manager, 1).await;
        submit_zisk_stream(&zisk_job_manager, 2).await;

        snark_job_manager
            .submit_proof(1, 2, proving_version, vec![0xAA; 8], "prover-1".into())
            .await
            .expect("optional mode must pass through");
        let cmd = rx.try_recv().expect("command sent downstream");
        let (_batches, proof) = cmd.into_parts();
        assert!(
            matches!(proof, SnarkProof::Real(_)),
            "optional mode must never send a MultiProof to L1"
        );
        assert!(
            !zisk_aggregation_job_manager.has_input(1).await
                && !zisk_aggregation_job_manager.has_input(2).await,
            "inputs swept"
        );
        assert!(
            zisk_aggregation_job_manager
                .pick_next_job("agg-1")
                .await
                .is_none(),
            "no aggregation job for batches already sent"
        );
    }

    /// Aggregated + wait timeout expired: the range is NOT degraded to
    /// Airbender-only; the submission hard-errors and the jobs stay
    /// re-offerable.
    #[tokio::test]
    async fn aggregated_wait_timeout_keeps_range_reofferable() {
        let (snark_job_manager, _zjm, _ajm, mut rx, proving_version) =
            aggregated_manager(true, Some(Duration::ZERO)).await;

        let err = snark_job_manager
            .submit_proof(1, 2, proving_version, vec![0xAA; 8], "prover-1".into())
            .await
            .expect_err("must not degrade to Airbender-only after the timeout");
        assert!(
            err.to_string().contains("Airbender submission is rejected"),
            "{err}"
        );
        assert!(rx.try_recv().is_err(), "nothing may be sent downstream");
        assert!(
            snark_job_manager
                .jobs
                .get_job_batch_metadata(1)
                .await
                .is_some(),
            "jobs must remain queued (re-offerable) after the timeout"
        );
    }
}
