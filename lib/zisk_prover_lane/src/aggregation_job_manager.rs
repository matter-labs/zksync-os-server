//! Job manager for the ZiSK AGGREGATION stage.
//!
//! Mirrors the Airbender FRI→SNARK split on the ZiSK lane: per-batch ZiSK
//! proving keeps the existing pick/submit flow (`ZiskJobManager`) but — in
//! aggregated mode — produces `vadcop_final` proof streams instead of
//! PLONK-wrapped SNARKs. This manager buffers those streams and collapses a
//! RANGE of them into one aggregation job: the aggregator guest
//! (`zksync-os-zisk/guest-aggregator`) verifies the N streams in-zkVM and
//! commits the L1 binding digest over their chained batch public inputs.
//!
//! # Range identity
//!
//! Aggregation ranges are exactly the batch ranges the Airbender SNARK
//! jobs cover — never independently counted. `SnarkJobManager` calls
//! [`Self::note_snark_range`] when it assigns a real SNARK range (early
//! start) and again on SNARK submission (authoritative), so the MultiProof
//! rendezvous can pair one Airbender range SNARK with one ZiSK range proof
//! of the same `[from..to]`. Because a timed-out SNARK range may be
//! re-picked with different bounds, several overlapping ranges can be
//! tracked at once; whichever the Airbender submission settles on wins,
//! and [`Self::take_completed`] retires everything it overlaps. Buffered
//! per-batch inputs are shared by overlapping ranges and live until a
//! range consumes them (rendezvous) or a discard passes them.
//!
//! An accepted aggregated proof is validated — sizes, the aggregator
//! guest's program-VK tripwire, and the binding digest recomputed from the
//! buffered per-batch streams — and parked in `completed` until the
//! Airbender SNARK for the same range takes it (`SnarkJobManager` is the
//! rendezvous point, exactly like the per-batch flow).

use alloy::primitives::{B256, keccak256};
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

use crate::commitment::ZISK_PUBLIC_VALUES_BYTES;
use crate::metrics::ZISK_LANE_METRICS;
use crate::persistence::ZiskAggregationPersistence;
use zksync_os_batch_types::batcher_model::ZISK_SNARK_PROOF_BYTES;

/// Cap on buffered per-batch inputs (~330 KiB each). When full, new
/// arrivals are dropped with a warning; the SNARK-side wait timeout is the
/// operator backstop if aggregation provers stay offline.
const MAX_BUFFERED_INPUTS: usize = 64;

/// Completed aggregated proofs parked for the rendezvous. Ranges are
/// consumed in order by the Airbender lane, so more than a couple parked
/// ranges means the Airbender lane is stuck — cap and complain.
const MAX_COMPLETED: usize = 16;

/// Give-up threshold for a range whose submitted proof fails binding-digest
/// validation. A DETERMINISTIC mismatch (a real divergence, or inputs the
/// aggregator cannot reconcile) would otherwise re-prove the whole range
/// forever. After this many misses the range is abandoned instead of
/// requeued. Mirrors the per-batch lane's `MAX_COMMITMENT_MISMATCH_ATTEMPTS`;
/// > 1 so a genuinely flaky prover still gets retries.
const MAX_DIGEST_MISMATCH_ATTEMPTS: u32 = 3;

/// One buffered per-batch aggregation input: the validated `vadcop_final`
/// stream plus the values extracted/validated at per-batch submission.
///
/// Derives serde so `ProofStorage` can save the input to disk and reload it on
/// restart. See the `save_zisk_*` / `load_zisk_*` methods in `proof_storage.rs`.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct AggregationInput {
    /// The full serialized proof stream the aggregator guest verifies.
    pub stream: Vec<u8>,
    /// Inner STF guest program VK (32-byte big-endian wire form).
    pub program_vk: B256,
    /// vadcop-final VK / rootCVadcopFinal (32-byte big-endian wire form).
    pub vadcop_vk: B256,
    /// The batch commitment the stream's publics carry — already validated
    /// against the batch metadata by `ZiskJobManager::submit_proof`.
    pub commitment: B256,
}

/// Outcome of offering a validated per-batch input to the aggregation buffer.
///
/// Distinguishes the two "not buffered" cases so the per-batch lane can keep a
/// transiently-rejected input re-tryable while dropping a permanently-useless
/// one, instead of collapsing both into one `false`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggregationInputOutcome {
    /// Buffered as range input (freshly inserted or already present).
    Buffered,
    /// Not buffered: the buffer is full. Transient backpressure — the input is
    /// still valid, so the caller must keep it re-tryable (the ZiSK job manager
    /// re-parks it in its backlog).
    BufferFull,
    /// Not buffered: the batch is at or below the consumed-range floor. Its
    /// range already went downstream, so the input can never join a range —
    /// the caller drops it.
    BelowFloor,
}

/// An aggregation job handed to a prover: the N `vadcop_final` streams of
/// a contiguous range, in batch order.
pub struct ZiskAggregationJob {
    pub from_batch: u64,
    pub to_batch: u64,
    pub streams: Vec<(u64, Vec<u8>)>,
}

/// A validated aggregated range proof parked until its Airbender SNARK
/// arrives (the same 768-byte SNARK + 320-byte public-values wire shape as
/// a per-batch proof; the binding digest sits at `public_values[32..64]`).
///
/// Derives serde so `ProofStorage` can save the proof to disk and reload it on
/// restart. See the `save_zisk_*` / `load_zisk_*` methods in `proof_storage.rs`.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct CompletedAggregatedProof {
    pub proof: Vec<u8>,
    pub public_values: Vec<u8>,
}

/// How much work the aggregation lane holds. Read-only observability, reported
/// by the operator status endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ZiskAggregationCounts {
    /// Per-batch `vadcop_final` streams buffered as range input.
    pub inputs_buffered: u64,
    /// Ranges registered by the Airbender SNARK lane whose proof is still
    /// missing: collecting inputs, formed, or assigned.
    pub ranges_in_flight: u64,
    /// Validated range proofs parked for the MultiProof rendezvous.
    pub range_proofs_completed: u64,
    /// Batches at or below this can no longer join a range.
    pub floor: u64,
}

/// Where a range currently is in the aggregation lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZiskAggregationRangeStatus {
    /// A validated aggregated proof is parked, ready for composition.
    Completed,
    /// The range is tracked: inputs are being collected, or an aggregation
    /// job is formed/assigned — a proof is on its way.
    InFlight,
    /// The range is not tracked.
    Unknown,
}

#[derive(Debug, thiserror::Error)]
pub enum ZiskAggregationSubmitError {
    #[error("unknown or unassigned range {from}..{to}")]
    UnknownRange { from: u64, to: u64 },
    #[error("invalid proof size: {got} bytes, expected {expected}")]
    InvalidProofSize { got: usize, expected: usize },
    #[error("invalid public values size: {got} bytes, expected {expected}")]
    InvalidPublicValuesSize { got: usize, expected: usize },
    #[error(
        "aggregator program VK mismatch: prover reported {reported}, server expects {expected}"
    )]
    VkDrift { reported: B256, expected: B256 },
    #[error("inner vadcop VK mismatch: range carries {reported}, server expects {expected}")]
    InnerVadcopVkDrift { reported: B256, expected: B256 },
    #[error("aggregated binding digest mismatch: {0}")]
    DigestMismatch(String),
    #[error("aggregated range proof verification failed: {0}")]
    ProofVerificationFailed(String),
}

struct State {
    /// Buffered per-batch inputs, keyed by batch number. Retained until a
    /// completed range is taken (rendezvous) or a discard passes them, so
    /// overlapping re-keyed ranges can reuse them.
    inputs: BTreeMap<u64, AggregationInput>,
    /// SNARK-derived ranges whose inputs are still incomplete.
    targets: BTreeSet<(u64, u64)>,
    /// Formed ranges (all inputs buffered) awaiting (re)assignment.
    pickable: VecDeque<(u64, u64)>,
    /// Assigned ranges: (prover id, assigned at).
    assigned: HashMap<(u64, u64), (String, Instant)>,
    /// Validated aggregated proofs awaiting their Airbender SNARK.
    completed: BTreeMap<(u64, u64), CompletedAggregatedProof>,
    /// Binding-digest mismatch counter per range. Bounds how many times a
    /// mismatching range is requeued before it is abandoned. Cleared when the
    /// range is accepted or retired.
    mismatch_attempts: HashMap<(u64, u64), u32>,
    /// Batches at or below this can never join a range: they were either
    /// consumed by a taken range or sent downstream without aggregation.
    floor: u64,
}

impl State {
    fn knows_range(&self, range: (u64, u64)) -> bool {
        self.targets.contains(&range)
            || self.pickable.contains(&range)
            || self.assigned.contains_key(&range)
            || self.completed.contains_key(&range)
    }

    /// Requeue a range after a failed digest validation, unless it has already
    /// missed `MAX_DIGEST_MISMATCH_ATTEMPTS` times — in which case abandon it
    /// (a deterministic mismatch would otherwise re-prove the range forever).
    /// Returns true when the range was abandoned.
    fn requeue_or_abandon(&mut self, range: (u64, u64)) -> bool {
        let attempts = self.mismatch_attempts.entry(range).or_insert(0);
        *attempts += 1;
        if *attempts >= MAX_DIGEST_MISMATCH_ATTEMPTS {
            self.mismatch_attempts.remove(&range);
            true
        } else {
            self.pickable.push_back(range);
            false
        }
    }

    /// Move every target whose inputs are all buffered into the pickable
    /// queue, in range order.
    fn form_ready_ranges(&mut self) {
        let ready: Vec<(u64, u64)> = self
            .targets
            .iter()
            .copied()
            .filter(|&(from, to)| (from..=to).all(|b| self.inputs.contains_key(&b)))
            .collect();
        for range in ready {
            self.targets.remove(&range);
            tracing::info!(
                from = range.0,
                to = range.1,
                "ZiSK aggregation range formed"
            );
            self.pickable.push_back(range);
        }
    }

    /// Drop all tracking for ranges starting at or below `batch_to` and
    /// all inputs at or below it, and advance the floor. Ranges straddling
    /// the cut are dropped whole (they can never rendezvous), but their
    /// above-the-cut inputs stay: re-keyed ranges may reuse them.
    fn retire_up_to(&mut self, batch_to: u64, reason: &str) {
        let stale = |&(from, _): &(u64, u64)| from <= batch_to;
        for range in self
            .targets
            .iter()
            .copied()
            .filter(stale)
            .collect::<Vec<_>>()
        {
            self.targets.remove(&range);
            tracing::info!(
                from = range.0,
                to = range.1,
                reason,
                "ZiSK aggregation range dropped"
            );
        }
        let dropped_pickable: Vec<(u64, u64)> =
            self.pickable.iter().copied().filter(stale).collect();
        for range in dropped_pickable {
            self.pickable.retain(|r| r != &range);
            tracing::info!(
                from = range.0,
                to = range.1,
                reason,
                "ZiSK aggregation range dropped"
            );
        }
        for range in self
            .assigned
            .keys()
            .copied()
            .filter(stale)
            .collect::<Vec<_>>()
        {
            self.assigned.remove(&range);
            tracing::info!(
                from = range.0,
                to = range.1,
                reason,
                "ZiSK aggregation range dropped"
            );
        }
        for range in self
            .completed
            .keys()
            .copied()
            .filter(stale)
            .collect::<Vec<_>>()
        {
            self.completed.remove(&range);
            tracing::info!(
                from = range.0,
                to = range.1,
                reason,
                "parked aggregated ZiSK proof dropped"
            );
        }
        let stale_inputs: Vec<u64> = self.inputs.range(..=batch_to).map(|(&b, _)| b).collect();
        for b in stale_inputs {
            self.inputs.remove(&b);
        }
        for range in self
            .mismatch_attempts
            .keys()
            .copied()
            .filter(stale)
            .collect::<Vec<_>>()
        {
            self.mismatch_attempts.remove(&range);
        }
        self.floor = self.floor.max(batch_to);
    }

    fn record_gauges(&self) {
        ZISK_LANE_METRICS
            .aggregation_inputs_buffered
            .set(self.inputs.len() as u64);
        ZISK_LANE_METRICS
            .aggregated_proofs_awaiting_snark
            .set(self.completed.len() as u64);
    }
}

/// Manages ZiSK aggregation jobs with the same pick/submit assignment
/// model as the other prover stages.
pub struct ZiskAggregationJobManager {
    state: Mutex<State>,
    /// Upper bound on range width; set to `prover_api.max_fris_per_snark`,
    /// so wider SNARK ranges cannot exist.
    range_size: u64,
    assignment_timeout: Duration,
    /// Expected AGGREGATOR guest program VK (`public_values[0..32]` of an
    /// aggregated proof). When set, a submission with a different VK is
    /// rejected and counted — the prover runs a different aggregator
    /// build. Unset: the reported VK is only logged.
    expected_program_vk: Option<B256>,
    /// Expected INNER vadcop-final VK (`rootCVadcopFinal`) shared by the
    /// range's per-batch inputs. When set, a range whose inputs carry a
    /// different vadcop VK is rejected and counted
    /// (`zisk_lane_aggregated_vadcop_vk_drift`). Mirrors the per-batch lane's
    /// vadcop tripwire; the same VK is pinned on L1. Unset: not checked here.
    expected_vadcop_vk: Option<B256>,
    /// Whether to run the off-chain proof verification on each aggregated-range
    /// submission. True (the production default) runs the native aggregated-
    /// range PLONK wire check. False skips only that cryptographic check; the
    /// binding-digest check above it always runs.
    proof_verification_enabled: bool,
    /// When set, buffered inputs and parked range proofs are also written to
    /// disk and reloaded on restart, so the aggregated lane's GPU artifacts
    /// survive a server restart. This is the shared proof store (node-side
    /// `ProofStorage`), set only when the second proof system is enabled.
    /// Unset: the lane stays fully in-memory.
    proof_storage: std::sync::Mutex<Option<Arc<dyn ZiskAggregationPersistence>>>,
}

impl ZiskAggregationJobManager {
    pub fn new(
        range_size: usize,
        assignment_timeout: Duration,
        expected_program_vk: Option<B256>,
        expected_vadcop_vk: Option<B256>,
        proof_verification_enabled: bool,
    ) -> Self {
        assert!(range_size >= 1, "aggregation range_size must be >= 1");
        Self {
            state: Mutex::new(State {
                inputs: BTreeMap::new(),
                targets: BTreeSet::new(),
                pickable: VecDeque::new(),
                assigned: HashMap::new(),
                completed: BTreeMap::new(),
                mismatch_attempts: HashMap::new(),
                floor: 0,
            }),
            range_size: range_size as u64,
            assignment_timeout,
            expected_program_vk,
            expected_vadcop_vk,
            proof_verification_enabled,
            proof_storage: std::sync::Mutex::new(None),
        }
    }

    /// Attach the durable proof store. From now on a buffered input and a
    /// parked range proof are also written to disk, and their in-memory removal
    /// also deletes them from disk. Called at startup only when the second
    /// proof system is enabled.
    pub fn set_proof_storage(&self, storage: Arc<dyn ZiskAggregationPersistence>) {
        *self.proof_storage.lock().expect("proof storage lock") = Some(storage);
    }

    fn proof_storage(&self) -> Option<Arc<dyn ZiskAggregationPersistence>> {
        self.proof_storage
            .lock()
            .expect("proof storage lock")
            .clone()
    }

    /// Save a buffered aggregation input to disk (best effort). A save failure
    /// never fails the submission; persistence only adds restart durability.
    async fn persist_input(&self, batch_number: u64, input: &AggregationInput) {
        if let Some(storage) = self.proof_storage()
            && let Err(e) = storage
                .save_zisk_aggregation_input(batch_number, input)
                .await
        {
            tracing::warn!(
                batch = batch_number,
                "failed to persist ZiSK aggregation input: {e}"
            );
        }
    }

    /// Save a parked aggregated range proof to disk (best effort).
    async fn persist_aggregated(
        &self,
        from_batch: u64,
        to_batch: u64,
        proof: &CompletedAggregatedProof,
    ) {
        if let Some(storage) = self.proof_storage()
            && let Err(e) = storage
                .save_zisk_aggregated_proof(from_batch, to_batch, proof)
                .await
        {
            tracing::warn!(
                from = from_batch,
                to = to_batch,
                "failed to persist aggregated ZiSK proof: {e}"
            );
        }
    }

    /// Delete a parked aggregated range proof from disk (best effort).
    async fn unpersist_aggregated(&self, from_batch: u64, to_batch: u64) {
        if let Some(storage) = self.proof_storage()
            && let Err(e) = storage
                .remove_zisk_aggregated_proof(from_batch, to_batch)
                .await
        {
            tracing::warn!(
                from = from_batch,
                to = to_batch,
                "failed to remove persisted aggregated ZiSK proof: {e}"
            );
        }
    }

    /// Drop persisted inputs and range proofs at or below `batch_to` (best
    /// effort), mirroring `State::retire_up_to`.
    async fn prune_storage_up_to(&self, batch_to: u64) {
        if let Some(storage) = self.proof_storage()
            && let Err(e) = storage.prune_zisk_up_to(batch_to).await
        {
            tracing::warn!(
                batch_to,
                "failed to prune persisted ZiSK aggregation state: {e}"
            );
        }
    }

    /// Repopulate one buffered input from disk on startup. Neither re-validates
    /// nor re-persists: the input was validated and saved before the restart.
    pub async fn restore_input(&self, batch_number: u64, input: AggregationInput) {
        let mut state = self.state.lock().await;
        state.inputs.insert(batch_number, input);
        state.record_gauges();
    }

    /// Repopulate one parked range proof from disk on startup. The range
    /// rendezvouses with its Airbender SNARK once that lane re-registers it.
    pub async fn restore_completed(
        &self,
        from_batch: u64,
        to_batch: u64,
        proof: CompletedAggregatedProof,
    ) {
        let mut state = self.state.lock().await;
        state.completed.insert((from_batch, to_batch), proof);
        state.record_gauges();
    }

    /// Register a batch range covered by an Airbender SNARK job. Called by
    /// `SnarkJobManager` at real-job pick time (so aggregation proving
    /// starts while the Airbender SNARK is still being computed) and again
    /// at SNARK submission (authoritative). Idempotent per range; ranges
    /// entirely at or below the floor are ignored.
    pub async fn note_snark_range(&self, from_batch: u64, to_batch: u64) {
        if from_batch > to_batch {
            tracing::error!(
                from = from_batch,
                to = to_batch,
                "invalid SNARK range ignored"
            );
            return;
        }
        let width = to_batch - from_batch + 1;
        if width > self.range_size {
            // The range width equals max_fris_per_snark, so a wider SNARK range
            // is a logic slip — track the range anyway (correctness of the
            // rendezvous over the size hint).
            tracing::warn!(
                from = from_batch,
                to = to_batch,
                range_size = self.range_size,
                "SNARK range wider than the aggregation range size"
            );
        }
        let mut state = self.state.lock().await;
        if to_batch <= state.floor {
            return;
        }
        let range = (from_batch, to_batch);
        if state.knows_range(range) {
            return;
        }
        tracing::info!(
            from = from_batch,
            to = to_batch,
            "ZiSK aggregation range registered"
        );
        state.targets.insert(range);
        state.form_ready_ranges();
    }

    /// Feed one accepted per-batch `vadcop_final` proof (called by
    /// `ZiskJobManager::submit_proof`; stream shape, program VK, and batch
    /// commitment were already validated there). Idempotent per batch;
    /// arrivals at or below the floor or beyond the buffer cap are dropped.
    ///
    /// Reports the [`AggregationInputOutcome`]. The per-batch lane uses it to
    /// decide what to do with the batch: a `Buffered` input parks a completion
    /// marker; a `BufferFull` input must stay re-tryable (re-parked); a
    /// `BelowFloor` input is dropped.
    pub async fn on_proof_completed(
        &self,
        batch_number: u64,
        input: AggregationInput,
    ) -> AggregationInputOutcome {
        let mut state = self.state.lock().await;
        if batch_number <= state.floor {
            tracing::warn!(
                batch = batch_number,
                floor = state.floor,
                "ZiSK aggregation: proof arrived below the range floor — batch can no longer join a range, dropping"
            );
            return AggregationInputOutcome::BelowFloor;
        }
        if state.inputs.contains_key(&batch_number) {
            return AggregationInputOutcome::Buffered;
        }
        if state.inputs.len() >= MAX_BUFFERED_INPUTS {
            tracing::warn!(
                batch = batch_number,
                buffered = state.inputs.len(),
                "ZiSK aggregation input buffer full — dropping proof (aggregation provers offline?)"
            );
            return AggregationInputOutcome::BufferFull;
        }
        tracing::debug!(batch = batch_number, "ZiSK aggregation input buffered");
        // Clone once for persistence, then release the state lock before the
        // disk write so a save never blocks the lane.
        let to_persist = input.clone();
        state.inputs.insert(batch_number, input);
        state.form_ready_ranges();
        state.record_gauges();
        drop(state);
        self.persist_input(batch_number, &to_persist).await;
        AggregationInputOutcome::Buffered
    }

    /// Whether a per-batch input is buffered for `batch_number`.
    pub async fn has_input(&self, batch_number: u64) -> bool {
        self.state.lock().await.inputs.contains_key(&batch_number)
    }

    /// How much work the aggregation lane holds.
    pub async fn queue_counts(&self) -> ZiskAggregationCounts {
        let state = self.state.lock().await;
        ZiskAggregationCounts {
            inputs_buffered: state.inputs.len() as u64,
            ranges_in_flight: (state.targets.len() + state.pickable.len() + state.assigned.len())
                as u64,
            range_proofs_completed: state.completed.len() as u64,
            floor: state.floor,
        }
    }

    /// Where a range currently is in the aggregation lane.
    pub async fn range_status(&self, from_batch: u64, to_batch: u64) -> ZiskAggregationRangeStatus {
        let state = self.state.lock().await;
        let range = (from_batch, to_batch);
        if state.completed.contains_key(&range) {
            ZiskAggregationRangeStatus::Completed
        } else if state.targets.contains(&range)
            || state.pickable.contains(&range)
            || state.assigned.contains_key(&range)
        {
            ZiskAggregationRangeStatus::InFlight
        } else {
            ZiskAggregationRangeStatus::Unknown
        }
    }

    /// Pick the next aggregation job: first re-offer timed-out
    /// assignments, then the oldest formed range.
    pub async fn pick_next_job(&self, prover_id: &str) -> Option<ZiskAggregationJob> {
        let now = Instant::now();
        let mut state = self.state.lock().await;

        // Return timed-out assigned ranges to the pickable queue.
        let timed_out: Vec<(u64, u64)> = state
            .assigned
            .iter()
            .filter(|(_, (_, at))| now.duration_since(*at) >= self.assignment_timeout)
            .map(|(&range, _)| range)
            .collect();
        for range in timed_out {
            if let Some((old_prover, _)) = state.assigned.remove(&range) {
                tracing::warn!(
                    from = range.0,
                    to = range.1,
                    old_prover,
                    "ZiSK aggregation job timed out, returning to queue"
                );
                state.pickable.push_back(range);
            }
        }

        state.form_ready_ranges();

        let (from, to) = state.pickable.pop_front()?;
        let streams: Option<Vec<(u64, Vec<u8>)>> = (from..=to)
            .map(|b| state.inputs.get(&b).map(|i| (b, i.stream.clone())))
            .collect();
        let Some(streams) = streams else {
            // Cannot happen: formation requires all inputs, and every path
            // that drops inputs also drops the ranges over them. Fail the
            // pick loudly instead of handing out a broken job.
            tracing::error!(
                from,
                to,
                "formed aggregation range lost its inputs — dropping"
            );
            return None;
        };
        state
            .assigned
            .insert((from, to), (prover_id.to_string(), now));
        tracing::info!(from, to, prover_id, "ZiSK aggregation job assigned");
        Some(ZiskAggregationJob {
            from_batch: from,
            to_batch: to,
            streams,
        })
    }

    /// Submit an aggregated range proof.
    ///
    /// Validates sizes, the aggregator-guest program-VK tripwire, and that
    /// the proof's committed digest (`public_values[32..64]`) equals the
    /// binding digest recomputed from the buffered per-batch streams, then
    /// parks the proof in `completed` for the Airbender SNARK submission
    /// path to compose the MultiProof (`take_completed`).
    pub async fn submit_proof(
        &self,
        from_batch: u64,
        to_batch: u64,
        proof: Vec<u8>,
        public_values: Vec<u8>,
        prover_id: &str,
    ) -> Result<(), ZiskAggregationSubmitError> {
        if proof.len() != ZISK_SNARK_PROOF_BYTES {
            return Err(ZiskAggregationSubmitError::InvalidProofSize {
                got: proof.len(),
                expected: ZISK_SNARK_PROOF_BYTES,
            });
        }
        if public_values.len() != ZISK_PUBLIC_VALUES_BYTES {
            return Err(ZiskAggregationSubmitError::InvalidPublicValuesSize {
                got: public_values.len(),
                expected: ZISK_PUBLIC_VALUES_BYTES,
            });
        }

        // Aggregator program VK tripwire — reject before touching the job,
        // so it stays assigned and times out back to the queue.
        let reported_vk = B256::from_slice(&public_values[..32]);
        if let Some(expected) = self.expected_program_vk {
            if reported_vk != expected {
                ZISK_LANE_METRICS.aggregated_vk_drift.inc();
                tracing::error!(
                    from = from_batch,
                    to = to_batch,
                    prover_id,
                    %reported_vk,
                    %expected,
                    "aggregator program VK drift — prover is running a different aggregator guest build"
                );
                return Err(ZiskAggregationSubmitError::VkDrift {
                    reported: reported_vk,
                    expected,
                });
            }
        } else {
            tracing::info!(
                from = from_batch,
                to = to_batch,
                %reported_vk,
                "aggregator program VK reported (no expected VK configured)"
            );
        }

        let mut state = self.state.lock().await;
        let range = (from_batch, to_batch);
        if state.assigned.remove(&range).is_none() {
            return Err(ZiskAggregationSubmitError::UnknownRange {
                from: from_batch,
                to: to_batch,
            });
        }

        // Recompute the binding digest from the per-batch inputs this
        // manager buffered (their commitments were validated against the
        // batch metadata at per-batch submission).
        let inputs: Vec<&AggregationInput> = match (from_batch..=to_batch)
            .map(|b| state.inputs.get(&b))
            .collect::<Option<Vec<_>>>()
        {
            Some(inputs) => inputs,
            None => {
                // Assigned ranges always have their inputs (see
                // `pick_next_job`); defensive so a logic slip fails loudly.
                return Err(ZiskAggregationSubmitError::UnknownRange {
                    from: from_batch,
                    to: to_batch,
                });
            }
        };
        // Inner vadcop-final VK (rootCVadcopFinal) tripwire: the range's
        // per-batch inputs all share one vadcop VK (the digest recompute
        // enforces this); reject the range if it is not the configured one.
        // Mirrors the per-batch program-VK tripwire. Copy the value out so the
        // `inputs` borrow ends before the state is mutated below.
        let reported_vadcop_vk = inputs.first().map(|i| i.vadcop_vk);
        let expected = match expected_aggregated_public_input(&inputs) {
            Ok(digest) => digest,
            Err(msg) => {
                if state.requeue_or_abandon(range) {
                    ZISK_LANE_METRICS.unprovable.inc();
                    tracing::error!(
                        from = from_batch,
                        to = to_batch,
                        attempts = MAX_DIGEST_MISMATCH_ATTEMPTS,
                        "ZiSK aggregation range unprovable: its buffered per-batch inputs \
                         could not form a binding digest on every attempt — giving up on this \
                         range (not requeued). Investigate the divergence. Sequencing is \
                         unaffected."
                    );
                }
                return Err(ZiskAggregationSubmitError::DigestMismatch(msg));
            }
        };
        if let (Some(expected_vk), Some(reported)) = (self.expected_vadcop_vk, reported_vadcop_vk)
            && reported != expected_vk
        {
            ZISK_LANE_METRICS.aggregated_vadcop_vk_drift.inc();
            tracing::error!(
                from = from_batch,
                to = to_batch,
                prover_id,
                %reported,
                expected = %expected_vk,
                "aggregated range inner vadcop VK drift — the per-batch inputs were proven \
                 under a different recursive setup than the configured rootCVadcopFinal"
            );
            // The range was already removed from `assigned`; requeue-or-abandon
            // like a digest mismatch so it is not lost but does not loop forever.
            if state.requeue_or_abandon(range) {
                ZISK_LANE_METRICS.unprovable.inc();
            }
            return Err(ZiskAggregationSubmitError::InnerVadcopVkDrift {
                reported,
                expected: expected_vk,
            });
        }
        let got = B256::from_slice(&public_values[32..64]);
        if got != expected {
            ZISK_LANE_METRICS.aggregated_digest_mismatches.inc();
            if state.requeue_or_abandon(range) {
                ZISK_LANE_METRICS.unprovable.inc();
                tracing::error!(
                    from = from_batch,
                    to = to_batch,
                    prover_id,
                    %got,
                    %expected,
                    attempts = MAX_DIGEST_MISMATCH_ATTEMPTS,
                    "ZiSK aggregation range unprovable: binding digest mismatched on every \
                     attempt (deterministic divergence) — giving up on this range (not \
                     requeued). One proof system disagrees; investigate. Sequencing is \
                     unaffected."
                );
            } else {
                tracing::error!(
                    from = from_batch,
                    to = to_batch,
                    prover_id,
                    %got,
                    %expected,
                    "aggregated ZiSK proof binding-digest mismatch — requeueing range"
                );
            }
            return Err(ZiskAggregationSubmitError::DigestMismatch(format!(
                "committed digest {got} does not match expected {expected}"
            )));
        }

        // Off-chain verification of the aggregated-range PLONK artifact (the
        // second gate; the binding digest above is the first). This is the
        // crate's aggregated-range entrypoint: it checks the 768/320 wire form
        // and confirms the public signal is derivable, exactly as the on-chain
        // ZiskVerifier decode does BEFORE the pairing. The final BN254 pairing
        // stays L1-verified. The aggregator program VK is already bound by the
        // tripwire above; the aggregator's `rootCVadcopFinal` is not configured
        // server-side, so the non-binding wire check is used here. The
        // `proof_verification_enabled` toggle can skip this cryptographic check;
        // the binding-digest check above always runs.
        if let Err(e) = if self.proof_verification_enabled {
            zisk_verifier::verify_aggregated_range(&proof, &public_values)
        } else {
            Ok(())
        } {
            ZISK_LANE_METRICS
                .aggregated_proof_verification_failures
                .inc();
            tracing::error!(
                from = from_batch,
                to = to_batch,
                prover_id,
                "aggregated ZiSK range proof rejected by off-chain verification: {e}"
            );
            // Reject: do not park it. The range was already removed from
            // `assigned`; requeue-or-abandon like a digest mismatch so it is not
            // lost but does not loop forever. Sequencing is unaffected.
            state.requeue_or_abandon(range);
            return Err(ZiskAggregationSubmitError::ProofVerificationFailed(
                e.to_string(),
            ));
        }

        tracing::info!(
            from = from_batch,
            to = to_batch,
            prover_id,
            aggregator_program_vk = %reported_vk,
            digest = %got,
            "aggregated ZiSK proof accepted, awaiting Airbender SNARK for multi-proof composition"
        );
        ZISK_LANE_METRICS.aggregated_proofs_accepted.inc();
        // Accepted: clear any prior mismatch attempts so the give-up counter
        // never carries over to a later re-keyed range with the same bounds.
        state.mismatch_attempts.remove(&range);
        let parked = CompletedAggregatedProof {
            proof,
            public_values,
        };
        // Clone once for persistence, then park the value in memory.
        let to_persist = parked.clone();
        state.completed.insert(range, parked);
        let mut evicted = Vec::new();
        while state.completed.len() > MAX_COMPLETED {
            let oldest = *state.completed.keys().next().expect("non-empty");
            state.completed.remove(&oldest);
            evicted.push(oldest);
            tracing::error!(
                from = oldest.0,
                to = oldest.1,
                "too many aggregated proofs awaiting their Airbender SNARK — dropping the oldest (Airbender lane stuck?)"
            );
        }
        state.record_gauges();
        // Release the state lock before touching disk.
        drop(state);
        self.persist_aggregated(from_batch, to_batch, &to_persist)
            .await;
        for (from, to) in evicted {
            self.unpersist_aggregated(from, to).await;
        }
        Ok(())
    }

    /// Take the validated aggregated proof for a range, if one is parked.
    /// Called by the Airbender SNARK submission path to compose the
    /// MultiProof. On success the consumed batches' inputs and every
    /// tracked range they overlap are retired, and the floor advances.
    pub async fn take_completed(
        &self,
        from_batch: u64,
        to_batch: u64,
    ) -> Option<CompletedAggregatedProof> {
        let taken = {
            let mut state = self.state.lock().await;
            let taken = state.completed.remove(&(from_batch, to_batch))?;
            state.retire_up_to(to_batch, "batches consumed by a composed multi-proof");
            state.record_gauges();
            taken
        };
        // Mirror the in-memory retirement on disk: the taken range's proof, and
        // every input and range at or below the consumed batch.
        self.prune_storage_up_to(to_batch).await;
        Some(taken)
    }

    /// Drop aggregation state for batches at or below `batch_to`: called
    /// when those batches were sent downstream without a multi-proof
    /// (shadow mode, degraded sends, fake-SNARK pass) — they can never
    /// join a rendezvous anymore. Ranges straddling the cut are dropped
    /// whole; inputs above the cut stay for future re-keyed ranges.
    pub async fn discard_up_to(&self, batch_to: u64) {
        {
            let mut state = self.state.lock().await;
            state.retire_up_to(batch_to, "batches sent downstream without aggregation");
            state.record_gauges();
        }
        self.prune_storage_up_to(batch_to).await;
    }
}

/// The aggregated range proof's expected public input — the aggregator
/// guest's binding digest, recomputed server-side from the buffered
/// per-batch inputs of a range (in batch order):
///
/// ```text
/// digest    = keccak256(innerProgramVK ‖ rootCVadcopFinal ‖ chainedPI)
/// chainedPI = _computeZKsyncOSHash(0, PI):   result = PI[0]
///             then per input: result = keccak256(result ‖ PI[i]) >> 32
/// PI[i]     = uint256(commitment_i) >> 32   (224-bit, big-endian words)
/// ```
///
/// Both VKs enter in their 32-byte big-endian wire forms. All batches must
/// share one inner (program VK, vadcop VK) pair — the guest enforces the
/// same rule. Must match `Aggregator::finalize` in
/// `zksync-os-zisk/guest-aggregator/src/lib.rs`; the cross-stack vector
/// (`guest-aggregator/BINDING_VECTOR.md`) pins the two together via
/// `binding_digest_matches_cross_stack_vector` below.
pub fn expected_aggregated_public_input(inputs: &[&AggregationInput]) -> Result<B256, String> {
    let Some((first, rest)) = inputs.split_first() else {
        return Err("empty range".into());
    };

    let mut chained = shr32(&first.commitment);
    for (i, input) in rest.iter().enumerate() {
        if input.program_vk != first.program_vk {
            return Err(format!(
                "batch #{} inner program VK differs within the range",
                i + 1
            ));
        }
        if input.vadcop_vk != first.vadcop_vk {
            return Err(format!(
                "batch #{} inner vadcop VK differs within the range",
                i + 1
            ));
        }
        let mut preimage = [0u8; 64];
        preimage[..32].copy_from_slice(chained.as_slice());
        preimage[32..].copy_from_slice(shr32(&input.commitment).as_slice());
        chained = shr32(&keccak256(preimage));
    }

    let mut binding = [0u8; 96];
    binding[..32].copy_from_slice(first.program_vk.as_slice());
    binding[32..64].copy_from_slice(first.vadcop_vk.as_slice());
    binding[64..].copy_from_slice(chained.as_slice());
    Ok(keccak256(binding))
}

/// A 32-byte big-endian uint256 right-shifted 32 bits — the contracts'
/// 224-bit public-input truncation, applied to per-batch public inputs and
/// to every chain step.
fn shr32(word: &B256) -> B256 {
    let mut out = [0u8; 32];
    out[4..].copy_from_slice(&word.as_slice()[..28]);
    B256::from(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const K: usize = 4;

    fn manager() -> ZiskAggregationJobManager {
        // The submitted aggregated-range proofs are well-shaped SNARK
        // artifacts, which pass the wire-form verification, so proof
        // verification stays on here.
        ZiskAggregationJobManager::new(K, Duration::from_secs(60), None, None, true)
    }

    /// A buffered input with the given commitment byte pattern and the
    /// shared test VKs. The stream payload is a small marker — this
    /// manager treats streams as opaque bytes (shape validation happens in
    /// `ZiskJobManager` before buffering).
    fn input(commitment_byte: u8) -> AggregationInput {
        AggregationInput {
            stream: vec![commitment_byte; 64],
            program_vk: B256::repeat_byte(0xA1),
            vadcop_vk: B256::repeat_byte(0xB2),
            commitment: B256::repeat_byte(commitment_byte),
        }
    }

    async fn feed(manager: &ZiskAggregationJobManager, batch: u64) {
        manager.on_proof_completed(batch, input(batch as u8)).await;
    }

    /// Aggregated public values for a range: the given digest at [32..64].
    fn aggregated_pv(digest: B256) -> Vec<u8> {
        let mut pv = vec![0u8; ZISK_PUBLIC_VALUES_BYTES];
        pv[32..64].copy_from_slice(digest.as_slice());
        pv
    }

    async fn expected_digest(manager: &ZiskAggregationJobManager, from: u64, to: u64) -> B256 {
        let state = manager.state.lock().await;
        let inputs: Vec<&AggregationInput> = (from..=to)
            .map(|b| state.inputs.get(&b).expect("input buffered"))
            .collect();
        expected_aggregated_public_input(&inputs).expect("digest")
    }

    /// THE cross-stack binding vector (real 4-batch aggregation session,
    /// ZiSK v0.18.0). The aggregator guest's `cross_stack_binding_vector`
    /// test and `zksync-os-zisk/guest-aggregator/BINDING_VECTOR.md` pin
    /// the same values. Update all pins together.
    #[test]
    fn binding_digest_matches_cross_stack_vector() {
        let program_vk: B256 = "0x481748830df5c3b7aa5522333ace2c4b533352637b92fd3c83ecc506c5104ead"
            .parse()
            .unwrap();
        let vadcop_vk: B256 = "0xcf2a309856f107b143836ada112806da71ae11567fa3f2d2050baba5381c7b7d"
            .parse()
            .unwrap();
        let commitments = [
            "0x95693fd871251f2a04f558f94852d31d4f7b0cd38b0ee2c746bd2851dc701dca",
            "0x4962160e4e0addc72fe2178dbbf3c5882ca1033790bb968d4fa451485987f99b",
            "0xe697864dd72ddded6f1818db6618efff8e695714db8492ac50abc9f5d8b6221e",
            "0x3cbda79d374329af945a0b1d2d73c87b2cd2cadb69ab3d6c03166a690dfff898",
        ];
        let inputs: Vec<AggregationInput> = commitments
            .iter()
            .map(|c| AggregationInput {
                stream: vec![],
                program_vk,
                vadcop_vk,
                commitment: c.parse().unwrap(),
            })
            .collect();
        let refs: Vec<&AggregationInput> = inputs.iter().collect();
        let digest = expected_aggregated_public_input(&refs).unwrap();
        assert_eq!(
            format!("{digest:#x}"),
            "0x5f47db9b336cf84b7b7fc49ca77eadb5160e373dc8f12057d719f45d3b2fbd84"
        );
    }

    /// A single-batch range binds the first public input unhashed
    /// (`initialHash == 0` seeds the chain with PI[0]).
    #[test]
    fn single_batch_digest_seeds_with_first_public_input() {
        let a = input(0x11);
        let digest = expected_aggregated_public_input(&[&a]).unwrap();
        let mut binding = [0u8; 96];
        binding[..32].copy_from_slice(a.program_vk.as_slice());
        binding[32..64].copy_from_slice(a.vadcop_vk.as_slice());
        binding[64..].copy_from_slice(shr32(&a.commitment).as_slice());
        assert_eq!(digest, keccak256(binding));
    }

    /// With an expected inner vadcop VK pinned, a range whose buffered
    /// per-batch inputs carry a different vadcop VK is rejected before the
    /// digest comparison. Inputs use vadcop VK 0xB2 (see `input`).
    #[tokio::test]
    async fn inner_vadcop_vk_drift_rejects_range() {
        let manager = ZiskAggregationJobManager::new(
            K,
            Duration::from_secs(60),
            None,
            Some(B256::repeat_byte(0xEE)),
            true,
        );
        manager.note_snark_range(1, 4).await;
        for batch in 1..=4u64 {
            feed(&manager, batch).await;
        }
        manager.pick_next_job("agg-1").await.expect("range 1..4");
        let err = manager
            .submit_proof(
                1,
                4,
                vec![0; ZISK_SNARK_PROOF_BYTES],
                aggregated_pv(B256::ZERO),
                "agg-1",
            )
            .await
            .expect_err("inner vadcop VK drift rejected");
        assert!(
            matches!(err, ZiskAggregationSubmitError::InnerVadcopVkDrift { .. }),
            "{err}"
        );
    }

    #[test]
    fn digest_rejects_mixed_inner_vks() {
        let a = input(0x11);
        let mut b = input(0x22);
        b.program_vk = B256::repeat_byte(0xFF);
        let err = expected_aggregated_public_input(&[&a, &b]).unwrap_err();
        assert!(err.contains("program VK"), "{err}");

        let mut c = input(0x22);
        c.vadcop_vk = B256::repeat_byte(0xFF);
        let err = expected_aggregated_public_input(&[&a, &c]).unwrap_err();
        assert!(err.contains("vadcop VK"), "{err}");
    }

    /// Ranges form only when noted by the SNARK lane — buffered inputs
    /// alone never form a job — and out-of-order per-batch completion
    /// still forms the range once the run is complete, in batch order.
    #[tokio::test]
    async fn ranges_form_only_when_noted() {
        let manager = manager();
        for batch in [9u64, 7, 10, 8] {
            feed(&manager, batch).await;
        }
        assert!(
            manager.pick_next_job("agg-1").await.is_none(),
            "inputs without a noted SNARK range must not form a job"
        );

        manager.note_snark_range(7, 10).await;
        let job = manager.pick_next_job("agg-1").await.expect("range formed");
        assert_eq!((job.from_batch, job.to_batch), (7, 10));
        let batches: Vec<u64> = job.streams.iter().map(|(b, _)| *b).collect();
        assert_eq!(batches, vec![7, 8, 9, 10]);
        assert!(
            manager.pick_next_job("agg-2").await.is_none(),
            "no second range"
        );
    }

    /// A noted range waits for its missing inputs and forms when the gap
    /// fills.
    #[tokio::test]
    async fn noted_range_waits_for_inputs() {
        let manager = manager();
        manager.note_snark_range(1, 4).await;
        for batch in [1u64, 2, 4] {
            feed(&manager, batch).await;
        }
        assert_eq!(
            manager.range_status(1, 4).await,
            ZiskAggregationRangeStatus::InFlight,
            "tracked while inputs are incomplete"
        );
        assert!(manager.pick_next_job("agg-1").await.is_none(), "gap at 3");
        feed(&manager, 3).await;
        let job = manager.pick_next_job("agg-1").await.expect("gap filled");
        assert_eq!((job.from_batch, job.to_batch), (1, 4));
    }

    /// The full lifecycle: note → feed → pick → submit → take. Taking the
    /// completed proof retires the consumed inputs (floor advances), so
    /// late arrivals below the floor are dropped and the next range
    /// continues cleanly.
    #[tokio::test]
    async fn lifecycle_and_floor_advance() {
        let manager = manager();
        manager.note_snark_range(5, 8).await;
        for batch in 5..=8u64 {
            feed(&manager, batch).await;
        }
        let job = manager.pick_next_job("agg-1").await.expect("range 5..8");
        let digest = expected_digest(&manager, 5, 8).await;
        manager
            .submit_proof(
                5,
                8,
                vec![0; ZISK_SNARK_PROOF_BYTES],
                aggregated_pv(digest),
                "agg-1",
            )
            .await
            .expect("valid aggregated proof accepted");
        assert_eq!((job.from_batch, job.to_batch), (5, 8));
        assert_eq!(
            manager.range_status(5, 8).await,
            ZiskAggregationRangeStatus::Completed
        );

        let taken = manager
            .take_completed(5, 8)
            .await
            .expect("parked proof taken");
        assert_eq!(taken.proof.len(), ZISK_SNARK_PROOF_BYTES);
        assert!(
            manager.take_completed(5, 8).await.is_none(),
            "taken exactly once"
        );
        assert_eq!(
            manager.range_status(5, 8).await,
            ZiskAggregationRangeStatus::Unknown
        );

        // Late arrival below the floor is dropped; the next range works.
        feed(&manager, 4).await;
        assert!(!manager.has_input(4).await);
        manager.note_snark_range(9, 12).await;
        for batch in 9..=12u64 {
            feed(&manager, batch).await;
        }
        let job = manager.pick_next_job("agg-1").await.expect("range 9..12");
        assert_eq!((job.from_batch, job.to_batch), (9, 12));
    }

    /// A timed-out SNARK range re-picked with different bounds: both
    /// ranges are tracked over the shared inputs, and whichever the
    /// Airbender submission settles on can rendezvous; taking it retires
    /// the overlapping alternative.
    #[tokio::test]
    async fn overlapping_rekeyed_ranges_share_inputs() {
        let manager = manager();
        manager.note_snark_range(1, 2).await;
        manager.note_snark_range(1, 4).await;
        for batch in 1..=4u64 {
            feed(&manager, batch).await;
        }

        let job_a = manager.pick_next_job("agg-1").await.expect("first range");
        let job_b = manager.pick_next_job("agg-2").await.expect("second range");
        let mut ranges = [
            (job_a.from_batch, job_a.to_batch),
            (job_b.from_batch, job_b.to_batch),
        ];
        ranges.sort();
        assert_eq!(ranges, [(1, 2), (1, 4)]);

        let digest = expected_digest(&manager, 1, 2).await;
        manager
            .submit_proof(
                1,
                2,
                vec![0; ZISK_SNARK_PROOF_BYTES],
                aggregated_pv(digest),
                "agg-1",
            )
            .await
            .expect("accepted");
        manager.take_completed(1, 2).await.expect("composed");
        // The overlapping (1,4) assignment is retired with the take.
        let digest = B256::ZERO;
        let err = manager
            .submit_proof(
                1,
                4,
                vec![0; ZISK_SNARK_PROOF_BYTES],
                aggregated_pv(digest),
                "agg-2",
            )
            .await
            .expect_err("retired range");
        assert!(matches!(
            err,
            ZiskAggregationSubmitError::UnknownRange { .. }
        ));
        // Batches 3..4 can still join a re-keyed range.
        assert!(manager.has_input(3).await && manager.has_input(4).await);
        manager.note_snark_range(3, 4).await;
        let job = manager
            .pick_next_job("agg-3")
            .await
            .expect("re-keyed range");
        assert_eq!((job.from_batch, job.to_batch), (3, 4));
    }

    /// Timeout reassignment: an assigned range whose prover vanished is
    /// re-offered with identical streams.
    #[tokio::test]
    async fn timeout_reassigns_range() {
        let manager = ZiskAggregationJobManager::new(K, Duration::ZERO, None, None, true);
        manager.note_snark_range(1, 4).await;
        for batch in 1..=4u64 {
            feed(&manager, batch).await;
        }
        let job_a = manager.pick_next_job("agg-a").await.expect("assigned to A");
        // Zero timeout: immediately reassignable.
        let job_b = manager
            .pick_next_job("agg-b")
            .await
            .expect("reassigned to B");
        assert_eq!(
            (job_b.from_batch, job_b.to_batch),
            (job_a.from_batch, job_a.to_batch)
        );
        assert_eq!(job_b.streams[0].1, job_a.streams[0].1);
    }

    /// Submissions for unknown/unassigned ranges are rejected; a wrong
    /// digest requeues the range for another prover; VK drift is rejected
    /// without consuming the assignment.
    #[tokio::test]
    async fn submit_validation() {
        let expected_vk = B256::repeat_byte(0x42);
        let manager = ZiskAggregationJobManager::new(
            K,
            Duration::from_secs(60),
            Some(expected_vk),
            None,
            true,
        );
        manager.note_snark_range(1, 4).await;
        for batch in 1..=4u64 {
            feed(&manager, batch).await;
        }

        let mut pv = aggregated_pv(B256::ZERO);
        pv[..32].copy_from_slice(expected_vk.as_slice());

        // Not picked yet -> unknown.
        let err = manager
            .submit_proof(1, 4, vec![0; ZISK_SNARK_PROOF_BYTES], pv.clone(), "agg-1")
            .await
            .expect_err("unassigned range");
        assert!(matches!(
            err,
            ZiskAggregationSubmitError::UnknownRange { .. }
        ));

        manager.pick_next_job("agg-1").await.expect("job");

        // Bad sizes.
        let err = manager
            .submit_proof(1, 4, vec![0; 3], pv.clone(), "agg-1")
            .await
            .expect_err("bad proof size");
        assert!(matches!(
            err,
            ZiskAggregationSubmitError::InvalidProofSize { .. }
        ));

        // Aggregator VK drift: rejected, assignment untouched.
        let mut drifted = pv.clone();
        drifted[..32].copy_from_slice(B256::repeat_byte(0x13).as_slice());
        let err = manager
            .submit_proof(1, 4, vec![0; ZISK_SNARK_PROOF_BYTES], drifted, "agg-1")
            .await
            .expect_err("VK drift");
        assert!(matches!(err, ZiskAggregationSubmitError::VkDrift { .. }));

        // Wrong digest -> rejected, range requeued and re-pickable.
        let err = manager
            .submit_proof(1, 4, vec![0; ZISK_SNARK_PROOF_BYTES], pv, "agg-1")
            .await
            .expect_err("wrong digest");
        assert!(matches!(err, ZiskAggregationSubmitError::DigestMismatch(_)));
        let requeued = manager
            .pick_next_job("agg-2")
            .await
            .expect("requeued range");
        assert_eq!((requeued.from_batch, requeued.to_batch), (1, 4));

        // Correct digest accepted.
        let digest = expected_digest(&manager, 1, 4).await;
        let mut pv = aggregated_pv(digest);
        pv[..32].copy_from_slice(expected_vk.as_slice());
        manager
            .submit_proof(1, 4, vec![0; ZISK_SNARK_PROOF_BYTES], pv, "agg-2")
            .await
            .expect("accepted");
        assert!(manager.take_completed(1, 4).await.is_some());
    }

    /// A deterministic binding-digest mismatch is given up on after
    /// `MAX_DIGEST_MISMATCH_ATTEMPTS` instead of re-proving the range forever:
    /// the range is requeued below the limit and abandoned (no longer pickable
    /// or tracked) at it.
    #[tokio::test]
    async fn persistent_digest_mismatch_gives_up() {
        let manager = manager();
        manager.note_snark_range(1, 4).await;
        for batch in 1..=4u64 {
            feed(&manager, batch).await;
        }
        let wrong = aggregated_pv(B256::ZERO);

        for attempt in 1..=MAX_DIGEST_MISMATCH_ATTEMPTS {
            manager
                .pick_next_job("agg-1")
                .await
                .expect("range pickable for retry");
            let err = manager
                .submit_proof(
                    1,
                    4,
                    vec![0; ZISK_SNARK_PROOF_BYTES],
                    wrong.clone(),
                    "agg-1",
                )
                .await
                .expect_err("wrong digest rejected");
            assert!(matches!(err, ZiskAggregationSubmitError::DigestMismatch(_)));
            if attempt < MAX_DIGEST_MISMATCH_ATTEMPTS {
                assert_eq!(
                    manager.range_status(1, 4).await,
                    ZiskAggregationRangeStatus::InFlight,
                    "requeued below the give-up threshold"
                );
            }
        }

        // Given up: the range is neither re-offered nor tracked.
        assert!(
            manager.pick_next_job("agg-1").await.is_none(),
            "an abandoned range is not re-offered"
        );
        assert_eq!(
            manager.range_status(1, 4).await,
            ZiskAggregationRangeStatus::Unknown,
            "an abandoned range is no longer tracked"
        );
    }

    /// `on_proof_completed` reports the outcome: `Buffered` for a fresh or
    /// already-present batch, `BelowFloor` for one whose range was consumed.
    /// The per-batch lane keys its completion-marker parking on this.
    #[tokio::test]
    async fn on_proof_completed_reports_outcome() {
        let manager = manager();
        assert_eq!(
            manager.on_proof_completed(5, input(5)).await,
            AggregationInputOutcome::Buffered,
            "a fresh input is buffered"
        );
        assert_eq!(
            manager.on_proof_completed(5, input(5)).await,
            AggregationInputOutcome::Buffered,
            "an already-buffered input reports present (idempotent)"
        );

        // Advance the floor past batch 5, then a late arrival is dropped.
        manager.note_snark_range(5, 5).await;
        let digest = expected_digest(&manager, 5, 5).await;
        manager.pick_next_job("agg-1").await.expect("range 5..5");
        manager
            .submit_proof(
                5,
                5,
                vec![0; ZISK_SNARK_PROOF_BYTES],
                aggregated_pv(digest),
                "agg-1",
            )
            .await
            .expect("accepted");
        manager.take_completed(5, 5).await.expect("composed");
        assert_eq!(
            manager.on_proof_completed(5, input(5)).await,
            AggregationInputOutcome::BelowFloor,
            "an input at or below the floor is dropped"
        );
    }

    /// Discards drop overlapping tracked ranges whole and advance the
    /// floor, but keep above-the-cut inputs for future re-keyed ranges.
    #[tokio::test]
    async fn discard_keeps_inputs_above_the_cut() {
        let manager = manager();
        manager.note_snark_range(3, 6).await;
        for batch in 3..=6u64 {
            feed(&manager, batch).await;
        }
        let job = manager.pick_next_job("agg-1").await.expect("range 3..6");
        assert_eq!((job.from_batch, job.to_batch), (3, 6));

        // The cut breaks the assigned range: the submit is rejected, but
        // inputs 5..6 survive for a re-keyed range.
        manager.discard_up_to(4).await;
        let err = manager
            .submit_proof(
                3,
                6,
                vec![0; ZISK_SNARK_PROOF_BYTES],
                aggregated_pv(B256::ZERO),
                "agg-1",
            )
            .await
            .expect_err("range dropped by discard");
        assert!(matches!(
            err,
            ZiskAggregationSubmitError::UnknownRange { .. }
        ));
        assert!(manager.has_input(5).await && manager.has_input(6).await);

        manager.note_snark_range(5, 6).await;
        let job = manager
            .pick_next_job("agg-1")
            .await
            .expect("re-keyed range 5..6");
        assert_eq!((job.from_batch, job.to_batch), (5, 6));

        // A parked completed proof overlapping a later cut is dropped too.
        let digest = expected_digest(&manager, 5, 6).await;
        manager
            .submit_proof(
                5,
                6,
                vec![0; ZISK_SNARK_PROOF_BYTES],
                aggregated_pv(digest),
                "agg-1",
            )
            .await
            .expect("accepted");
        manager.discard_up_to(6).await;
        assert!(manager.take_completed(5, 6).await.is_none());
    }

    /// Feeding the same batch twice keeps the first input (idempotence),
    /// and re-noting a known range is a no-op.
    #[tokio::test]
    async fn duplicate_feed_and_note_are_idempotent() {
        let manager = manager();
        manager.note_snark_range(1, 1).await;
        manager.note_snark_range(1, 1).await;
        manager.on_proof_completed(1, input(0xAA)).await;
        manager.on_proof_completed(1, input(0xBB)).await;
        let job = manager.pick_next_job("agg-1").await.expect("range");
        assert_eq!(job.streams[0].1, vec![0xAA; 64]);
        assert!(
            manager.pick_next_job("agg-2").await.is_none(),
            "no duplicate range"
        );
    }
}
