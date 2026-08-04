//! Multi-proof combination for the second proof system (ZiSK).
//!
//! The Airbender SNARK submission is the combination point. ZiSK proving starts
//! at batch seal (the batcher opens the ZiSK job, independent of the Airbender
//! FRI lane). When the Airbender SNARK arrives, this module composes a
//! MultiProof on the spot.
//!
//! The ZiSK lane always aggregates. The Airbender SNARK covers a batch range
//! and pairs with ONE aggregated ZiSK range proof of the same bounds, for any
//! range width — including a range of one batch. So L1 needs only one ZiSK
//! verification key (the aggregator's). This mirrors Airbender, which uses one
//! verification key for any range width. A single-batch range forms an
//! aggregation range of one; the aggregator guest binds it to the same digest a
//! wider range would, and the L1 aggregation verifier accepts any width. The
//! SNARK job's range is the range identity, reported to the aggregation manager
//! at pick time and again at submission.
//!
//! Under `require_multi_proof` a missing ZiSK proof blocks the submission (the
//! job is re-offered) for only the residual proving time. Otherwise the
//! Airbender-only proof goes downstream at once.
//!
//! The `require_multi_proof = false` path is *shadow proving*. The daemon
//! proves on the GPU and the server verifies the proof. But the server does
//! not send the MultiProof to L1. This is not *shadow equivalence*
//! (`zisk_shadow_execution`), which re-executes on the CPU and compares the
//! batch commitment. Shadow equivalence does not prove.
//!
//! Settlement runs ahead of the ZiSK lane in shadow proving, so a settled range
//! keeps its place in that lane: it still forms, it is still picked, and it is
//! still verified server-side, only late. The ZiSK job managers hold the mode
//! and apply it in `on_batches_settled`.
//!
//! All of this runs only when the ZiSK lane is configured; the shared
//! `SnarkJobManager` keeps a single gated call site.

use super::metrics::ProverType;
use super::snark_job_manager::SnarkJobManager;
use zisk_prover_lane::{ZISK_LANE_METRICS, ZiskAggregationRangeStatus, compose_multiproof};
use zksync_os_batch_types::batcher_model::SnarkProof;
use zksync_os_batcher_metrics::BatchExecutionStage;
use zksync_os_l1_sender::commands::prove::ProofCommand;
use zksync_os_types::ProvingVersion;

impl SnarkJobManager {
    /// Aggregated multi-proof submission. Entered only when the ZiSK job
    /// manager is set (which implies the aggregation manager is set too). The
    /// Airbender range SNARK pairs with the aggregated ZiSK proof of exactly
    /// `[batch_from..batch_to]`.
    ///
    /// The submitted range is the authoritative range identity: it is
    /// registered with the aggregation manager (idempotent — normally the
    /// pick already did), and under `require_multi_proof` the submission
    /// blocks (job re-offered) until the aggregated proof for that exact
    /// range is parked. A range of one batch is valid: the aggregator guest
    /// and the L1 aggregation verifier both accept any range width.
    pub(super) async fn submit_with_multiproof_combine(
        &self,
        batch_from: u64,
        batch_to: u64,
        proving_version: ProvingVersion,
        payload: Vec<u8>,
        prover_id: String,
    ) -> anyhow::Result<()> {
        let zisk_aggregation_job_manager = self
            .zisk_aggregation_job_manager
            .as_ref()
            .expect("multi-proof combine implies an aggregation manager");
        zisk_aggregation_job_manager
            .note_snark_range(batch_from, batch_to)
            .await;

        if self.require_multi_proof {
            let blocked_reason = match zisk_aggregation_job_manager
                .range_status(batch_from, batch_to)
                .await
            {
                ZiskAggregationRangeStatus::Completed => None,
                ZiskAggregationRangeStatus::InFlight => {
                    // The per-batch inputs are still being proved. Any that were
                    // parked at seal (active queue full) are promoted into
                    // active jobs by the ZiSK job manager as slots free, so
                    // their streams still arrive without a re-creation step.
                    Some("the aggregated ZiSK proof for the range has not been submitted yet")
                }
                // note_snark_range above tracks every range whose batches
                // are still in the job map, so this cannot happen.
                ZiskAggregationRangeStatus::Unknown => {
                    Some("the range is not tracked by the aggregation stage")
                }
            };
            if let Some(reason) = blocked_reason {
                let wait_expired = match self.multi_proof_wait_timeout {
                    None => false,
                    Some(timeout) => self
                        .jobs
                        .get_job_age(batch_from)
                        .await
                        .is_some_and(|age| age >= timeout),
                };
                // Never consume the range and send an Airbender-only (type-2)
                // proof — the type-5-only MultiProofVerifier rejects it and the
                // batches would be stranded. Hard-error whether or not the
                // timeout expired, leaving the jobs re-offerable.
                if wait_expired {
                    ZISK_LANE_METRICS.degraded_to_single_proof.inc();
                    tracing::error!(
                        batch_from,
                        batch_to,
                        reason,
                        "multi-proof wait timeout expired — keeping the range queued and \
                         re-offerable; an Airbender-only proof would be rejected on L1 by the \
                         MultiProofVerifier, so it is NOT sent"
                    );
                } else {
                    ZISK_LANE_METRICS.blocked_submits.inc();
                    tracing::warn!(
                        batch_from,
                        batch_to,
                        reason,
                        "multi-proof required — rejecting Airbender-only submission; \
                         the job stays queued and will be re-offered"
                    );
                }
                anyhow::bail!(
                    "multi_proof_verifier requires an aggregated ZiSK proof for batches \
                     {batch_from}..{batch_to} but {reason}; the Airbender submission is \
                     rejected and the jobs will be re-offered (an Airbender-only proof is not \
                     submitted because the MultiProofVerifier would reject it)"
                );
            }
        }

        // Ensure we can send downstream before consuming jobs from the
        // retryable map.
        let permit = self.try_reserve_permit_downstream()?;

        let Some(consumed_batches_proven) = self
            .jobs
            .complete_many_jobs(batch_from, batch_to, ProverType::Real, &prover_id)
            .await
        else {
            anyhow::bail!("race condition: some batches were completed earlier")
        };
        let consumed_batches_proven: Vec<_> = consumed_batches_proven
            .into_iter()
            .map(|b| b.with_stage(BatchExecutionStage::SnarkProvedReal))
            .collect();

        // The combination. In optional shadow-proving mode the MultiProof must
        // never reach L1 (`prove.rs` always encodes it as a type-5 payload,
        // which needs the MultiProofVerifier deployed): the aggregated
        // proof's submit-time digest validation is the shadow signal, and
        // the batches go downstream Airbender-only.
        if self.require_multi_proof
            && let Some(aggregated) = zisk_aggregation_job_manager
                .take_completed(batch_from, batch_to)
                .await
        {
            tracing::info!(
                batch_from,
                batch_to,
                airbender_proof_bytes = payload.len(),
                zisk_proof_bytes = aggregated.proof.len(),
                "Airbender range SNARK received, composing Airbender + aggregated ZiSK multi-proof"
            );
            // The ZiSK public values are not carried in the L1 payload; the
            // on-chain MultiProofVerifier reconstructs and binds the aggregated
            // proof's range digest, so no per-batch cross-check runs here.
            permit.send(ProofCommand::new(
                consumed_batches_proven,
                SnarkProof::MultiProof(compose_multiproof(
                    payload,
                    aggregated.proof,
                    proving_version,
                )),
            ));
            // Sweep the consumed batches' per-batch completion markers
            // (and, via the sink forward, any leftover aggregation state).
            if let Some(zisk_job_manager) = self.zisk_job_manager.as_ref() {
                zisk_job_manager.on_batches_settled(batch_to).await;
            }
            return Ok(());
        }

        // Airbender-only. The ZiSK lane is told the batches settled and applies
        // its mode: required drops what can no longer compose, shadow keeps the
        // range proving so the late verification still happens and is counted.
        if let Some(zisk_job_manager) = self.zisk_job_manager.as_ref() {
            zisk_job_manager.on_batches_settled(batch_to).await;
        }
        self.send_airbender_only(permit, consumed_batches_proven, payload, proving_version)
    }
}
