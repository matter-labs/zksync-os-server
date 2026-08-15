//! Metrics for the ZiSK proving lane.
//!
//! These carry the `zisk_lane` prefix they had when the lane lived in
//! `node/bin`, so the emitted series are unchanged. The Airbender FRI/SNARK
//! and proof-storage metrics stay in `node/bin`.

use std::time::Duration;
use vise::{Buckets, EncodeLabelSet, EncodeLabelValue, Family, Gauge, Histogram, Metrics, Unit};

/// Why a parked input left the [`crate::ZiskJobManager`] backlog without being
/// promoted into an active job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EncodeLabelValue, EncodeLabelSet)]
#[metrics(rename_all = "snake_case", label = "reason")]
pub enum ZiskBacklogEvictionReason {
    /// Entry exceeded the backlog max age before its active job could open.
    Expired,
    /// The backlog exceeded its max-entries bound; the oldest entry was dropped.
    Overflow,
}

#[derive(Debug, Metrics)]
#[metrics(prefix = "zisk_lane")]
pub struct ZiskLaneMetrics {
    /// ZiSK public values disagreed with the batch commitment — one proof
    /// system is wrong. The headline divergence alarm: page, don't just log.
    pub commitment_mismatches: vise::Counter,
    /// A batch was GIVEN UP on in continue mode: its ZiSK proof mismatched the
    /// batch commitment on every retry (a deterministic divergence), so the
    /// job was dropped instead of requeued forever. Distinct from
    /// `commitment_mismatches` (which also counts transient/faulty-prover
    /// misses): this fires once per abandoned batch and means the ZiSK lane
    /// cannot prove that batch. Sequencing is unaffected; investigate the
    /// divergence.
    pub unprovable: vise::Counter,
    /// Airbender SNARK submissions rejected (job left in place) because the
    /// batch's ZiSK proof path was unavailable while multi-proof is required.
    pub blocked_submits: vise::Counter,
    /// A ZiSK prover submitted a proof whose embedded program VK differs
    /// from the server's expected one — the prover is running a different
    /// guest build. Fires only when the batch's protocol version has a
    /// `zisk_vks` entry.
    pub vk_drift: vise::Counter,
    /// A ZiSK prover submitted a proof whose embedded inner vadcop-final VK
    /// (`rootCVadcopFinal`) differs from the server's expected one — the
    /// prover is running a different recursive setup. Fires only when the
    /// batch's protocol version has a `zisk_vks` entry.
    pub vadcop_vk_drift: vise::Counter,
    /// A per-batch ZiSK proof failed the server's off-chain verification after
    /// the commitment binding passed. In the per-batch PLONK lane this is the
    /// wire-form and program binding check; in the aggregated lane this is the
    /// native `vadcop_final` STARK verification. The proof is rejected and not
    /// parked. A rejection means a broken prover; the per-batch PLONK pairing
    /// stays L1-verified.
    pub proof_verification_failures: vise::Counter,
    /// A cryptographically verified submission disagreed with the expected
    /// commitment while this node's own seal-time guest execution agreed with
    /// it. The prover is at fault, not the proof system: the job is requeued
    /// for another prover and no divergence alarm fires. A rising count with a
    /// single prover behind it means that prover is broken or hostile.
    pub wrong_result_submissions: vise::Counter,
    /// Batch-level ZiSK witness builds attempted at seal. Incremented only
    /// when the second proof system is enabled, so it stays 0 when the feature
    /// is off — the disabled-equals-upstream signal.
    pub batch_witness_attempts: vise::Counter,
    /// ZiSK jobs waiting to be picked by a prover.
    pub jobs_pending: Gauge<u64>,
    /// ZiSK jobs assigned to provers, awaiting proof submission.
    pub jobs_assigned: Gauge<u64>,
    /// Validated ZiSK proofs parked awaiting their Airbender SNARK for
    /// multi-proof composition (the rendezvous buffer).
    pub proofs_awaiting_snark: Gauge<u64>,
    /// Age of the oldest ZiSK job (pending or assigned) in seconds.
    pub oldest_job_age_seconds: Gauge<u64>,
    /// Sealed-batch inputs parked in the backlog because the active queue was
    /// full. Each is promoted into an active job when a slot frees.
    pub backlog_entries: Gauge<u64>,
    /// Parked inputs evicted from the backlog by reason. Every eviction is a
    /// sealed batch that loses its ZiSK proof path without a re-seal.
    pub backlog_evictions: Family<ZiskBacklogEvictionReason, vise::Counter>,
    /// Time from ZiSK job creation (batch seal) to an accepted ZiSK proof
    /// submission.
    #[metrics(unit = Unit::Seconds, buckets = Buckets::LATENCIES)]
    pub time_to_submit: Histogram<Duration>,
    /// Wall-clock of the in-process guest re-execution per batch when
    /// `zisk_shadow_execution` is enabled.
    #[metrics(unit = Unit::Seconds, buckets = Buckets::LATENCIES)]
    pub shadow_execution_time: Histogram<Duration>,
    /// An aggregation prover submitted a range proof whose embedded
    /// aggregator program VK differs from the server's expected one.
    /// Fires only when `zisk_aggregation.program_vk` is configured.
    pub aggregated_vk_drift: vise::Counter,
    /// An aggregation range's buffered per-batch inputs carry an inner
    /// vadcop-final VK (`rootCVadcopFinal`) differing from the server's
    /// expected one. Fires only when the input's protocol version has a
    /// `zisk_vks` entry.
    pub aggregated_vadcop_vk_drift: vise::Counter,
    /// An aggregated range proof's committed binding digest disagreed with
    /// the digest recomputed from the buffered per-batch proofs.
    pub aggregated_digest_mismatches: vise::Counter,
    /// An aggregated range proof failed the server's off-chain verification
    /// (the wire-form and aggregator program binding check) after the binding
    /// digest matched. The proof is rejected and not parked. The range PLONK
    /// pairing stays L1-verified.
    pub aggregated_proof_verification_failures: vise::Counter,
    /// Server-side aggregation verification attempts returned to the pending
    /// queue after exceeding `verification_timeout`.
    pub aggregation_verification_timeouts: vise::Counter,
    /// Per-batch or aggregation submissions whose captured lease generation
    /// was replaced before they could commit their result.
    pub superseded_submissions: vise::Counter,
    /// Aggregated range proofs accepted and parked for the rendezvous.
    pub aggregated_proofs_accepted: vise::Counter,
    /// ZiSK work dropped before any range proof verified it: a sealed input
    /// evicted under a queue bound, a buffered input retired with a range that
    /// was never proven, or a validated per-batch proof that arrived after its
    /// range went downstream. One event per batch-level item, so the count
    /// approximates the number of settled batches left with no second-proof
    /// coverage. Any growth means coverage is being lost — page on it.
    pub coverage_lost: vise::Counter,
    /// Aggregated ranges verified AFTER their batches settled on L1. Shadow
    /// proving only: settlement there never waits for this lane, so a lagging
    /// range keeps proving and its verification outcome is measured. Growth
    /// tracks how far the ZiSK lane runs behind the Airbender lane; it is not
    /// an error.
    pub ranges_verified_after_settlement: vise::Counter,
    /// Per-batch `vadcop_final` streams buffered as aggregation inputs.
    pub aggregation_inputs_buffered: Gauge<u64>,
    /// Validated aggregated range proofs parked awaiting their Airbender
    /// SNARK for multi-proof composition.
    pub aggregated_proofs_awaiting_snark: Gauge<u64>,
}

#[vise::register]
pub static ZISK_LANE_METRICS: vise::Global<ZiskLaneMetrics> = vise::Global::new();
