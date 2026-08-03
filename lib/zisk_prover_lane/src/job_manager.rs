//! Job manager for per-batch ZiSK proof generation.
//!
//! Mirrors `FriJobManager` in architecture:
//! - Batches enter via `add_job` **when the batch is sealed** (the batcher
//!   opens the ZiSK job directly from the sealed batch), so ZiSK proving runs
//!   concurrently with the Airbender FRI + SNARK lane instead of serialized
//!   behind it. `add_job` never blocks the batcher: if the active queue is
//!   full it parks the input in a bounded backlog and promotes it, lowest
//!   batch first, when a slot frees. A sealed batch's input is never lost
//!   in-process, so a required-mode range never permanently stalls from a
//!   full-queue drop.
//! - External provers pick jobs via `pick_next_job` (with timeout-based
//!   reassignment) and submit proofs via `submit_proof`.
//! - An accepted proof is validated (shape, program-VK tripwire, batch
//!   commitment, then an off-chain proof check with `zisk-verifier`) and parked
//!   in the `completed` map. The batch commitment is the first gate; the
//!   off-chain proof check is the second. The per-batch final PLONK pairing
//!   stays L1-verified.
//!
//! The manager accepts one submission shape, fixed at startup:
//!
//! - **Aggregated mode** (marked by the attached
//!   aggregation sink): the daemon submits the raw `vadcop_final` proof
//!   stream (~330 KiB) instead. The validated stream is buffered in the
//!   aggregation manager as range input AND parked here mode-tagged (for
//!   idempotence and lane status); the MultiProof rendezvous then pairs
//!   the Airbender range SNARK with the aggregated range proof, not with
//!   per-batch proofs.
//!
//! This manager never sends downstream itself; composition and the send
//! permit live in `SnarkJobManager`.

use crate::aggregation_job_manager::AggregationInput;
use crate::metrics::{ZISK_LANE_METRICS, ZiskBacklogEvictionReason};
use crate::vadcop_stream::{ZISK_VADCOP_STREAM_BYTES, parse_vadcop_final_stream};
use alloy::primitives::B256;
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::Mutex;
use zisk_witness::ZiskChainConfig;
use zksync_os_batch_types::batcher_model::BatchMetadata;
use zksync_os_types::ProtocolSemanticVersion;

/// Maximum number of pending + assigned + completed-awaiting-SNARK ZiSK jobs.
/// Prevents unbounded memory growth if ZiSK provers are slow or offline, and
/// bounds completed proofs parked while the Airbender lane lags. When full,
/// `add_job` parks the input in the backlog (below) instead of activating it.
const MAX_TOTAL_JOBS: usize = 50;

/// Maximum sealed-batch inputs held in the parked backlog — the inputs of
/// batches whose active job could not open because the queue was full. This is
/// the same bound the previous out-of-band data cache used, so memory behaviour
/// is unchanged. A backlog entry is a batch whose ZiSK proof path is still
/// open; evicting it drops that path, so the batch then needs a re-seal to
/// prove.
const MAX_BACKLOG_ENTRIES: usize = 100;

/// A parked input older than this is evicted. By then its Airbender SNARK has
/// almost certainly already passed, so its range can no longer compose.
const MAX_BACKLOG_AGE: Duration = Duration::from_secs(86400); // 24 hours

/// Continue-mode give-up threshold: after this many commitment mismatches for
/// the same batch, the job is abandoned instead of requeued. A DETERMINISTIC
/// divergence (a real bug in one proof system, or a batch the guest cannot
/// reproduce) would otherwise requeue forever — `on_batches_settled` only
/// sweeps `completed`, never a requeued `pending` job — leaking a
/// `MAX_TOTAL_JOBS` slot until ZiSK coverage stops. Small enough to free
/// the slot promptly; > 1 so a genuinely flaky prover still gets retries.
const MAX_COMMITMENT_MISMATCH_ATTEMPTS: u32 = 3;

/// Data stored per ZiSK job, captured at batch seal.
pub struct ZiskJobData {
    /// Bincode-serialized BatchInput for cargo-zisk.
    pub zisk_data: Vec<u8>,
    /// Batch metadata captured at job creation: VK hash for pick, commitment
    /// preimages (previous state commitment + batch info) for submit-time
    /// proof validation.
    pub batch_metadata: BatchMetadata,
    /// When the job was created (batch seal). Preserved across backlog parking,
    /// promotion, and requeues, so `zisk_lane_time_to_submit` measures total
    /// wall-clock from job creation to accepted proof, and so the backlog ages
    /// entries from seal time.
    pub added_at: std::time::Instant,
}

/// A validated per-batch ZiSK proof parked in the `completed` map,
/// mode-tagged by what the daemon submitted. Held in memory only: the durable
/// artifact is the aggregation input, which the aggregation manager persists.
/// Where a batch currently is in the ZiSK lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZiskBatchStatus {
    /// A validated proof is parked, ready for MultiProof composition.
    Completed,
    /// A job is pending or assigned — a proof is on its way.
    InFlight,
    /// No job and no proof for this batch.
    Unknown,
}

/// How much work the per-batch lane holds, by lifecycle stage. Read-only
/// observability: the operator status endpoint reports it, and a test asserts
/// that submissions were accepted (`proofs_completed`) rather than dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ZiskQueueCounts {
    pub jobs_pending: u64,
    pub jobs_assigned: u64,
    /// Validated proofs parked for the MultiProof rendezvous.
    pub proofs_completed: u64,
    /// Sealed inputs parked because the active queue was full.
    pub inputs_in_backlog: u64,
}

/// Job metadata returned to the prover on pick.
pub struct ZiskJob {
    pub batch_number: u64,
    pub vk_hash: String,
    pub zisk_data: Vec<u8>,
}

/// Errors from ZiSK proof submission.
#[derive(Debug, thiserror::Error)]
pub enum ZiskSubmitError {
    /// The aggregation sink is not wired. The server attaches it at startup;
    /// a submission before that point is a wiring error.
    #[error("the ZiSK aggregation sink is not wired; the server attaches it at startup")]
    AggregationNotWired,
    #[error("unknown batch {0}")]
    UnknownJob(u64),
    #[error("invalid proof size: {got} bytes, expected {expected}{hint}")]
    InvalidProofSize {
        got: usize,
        expected: usize,
        hint: &'static str,
    },
    #[error("invalid public values size: {got} bytes, expected {expected}")]
    InvalidPublicValuesSize { got: usize, expected: usize },
    #[error("malformed vadcop_final proof stream: {0}")]
    MalformedProof(String),
    #[error("batch commitment mismatch: ZiSK proof public values do not match batch commitment")]
    CommitmentMismatch,
    #[error("program VK mismatch: prover reported {reported}, server expects {expected}")]
    VkDrift { reported: B256, expected: B256 },
    #[error("vadcop VK mismatch: prover reported {reported}, server expects {expected}")]
    VadcopVkDrift { reported: B256, expected: B256 },
    #[error("ZiSK proof verification failed: {0}")]
    ProofVerificationFailed(String),
}

/// Inner state protected by a single mutex to avoid lock ordering issues.
struct ZiskJobState {
    pending: HashMap<u64, ZiskJobData>,
    assigned: HashMap<u64, (String, std::time::Instant, ZiskJobData)>,
    /// Validated proofs awaiting composition (per-batch rendezvous in PLONK
    /// mode; completion markers in aggregated mode).
    /// Validated `vadcop_final` streams parked per batch. The entry marks
    /// the batch completed, so re-adding the batch stays idempotent and the
    /// lane status is accurate; composition happens at range level via the
    /// aggregation manager, which buffered its own copy as range input.
    completed: HashMap<u64, Vec<u8>>,
    /// Continue-mode commitment-mismatch counter per batch. Bounds how many
    /// times a mismatching batch is requeued before it is abandoned.
    /// Not part of `total()`: it holds no job, only an attempt count, and is
    /// cleared when the batch is accepted, given up on, or discarded.
    mismatch_attempts: HashMap<u64, u32>,
    /// Sealed-batch inputs parked because the active queue was full at
    /// `add_job`. Bounded (`MAX_BACKLOG_ENTRIES` / `MAX_BACKLOG_AGE`), promoted
    /// into `pending` lowest batch first when a slot frees. Not part of
    /// `total()`: a parked input holds no active slot. This is the in-process
    /// backpressure buffer that lets `add_job` stay non-blocking without
    /// dropping a sealed batch's input.
    backlog: HashMap<u64, ZiskJobData>,
}

impl ZiskJobState {
    fn total(&self) -> usize {
        self.pending.len() + self.assigned.len() + self.completed.len()
    }

    fn knows(&self, batch_number: u64) -> bool {
        self.pending.contains_key(&batch_number)
            || self.assigned.contains_key(&batch_number)
            || self.completed.contains_key(&batch_number)
    }
}

/// The expected ZiSK verification keys of one protocol version's STF guest
/// build: the program VK (public values `[0..32]`) and the inner vadcop-final
/// VK / `rootCVadcopFinal` (public values `[288..320]`, or the `vadcop_final`
/// stream tail in aggregated mode). Both are pinned together because a guest
/// build fixes both at once.
#[derive(Debug, Clone, Copy)]
pub struct ZiskVkSet {
    pub program_vk: B256,
    pub vadcop_vk: B256,
}

/// Whether the multi-proof gates L1 settlement. Fixed at startup from
/// `multi_proof_verifier` and given to both ZiSK job managers, because it
/// decides what a settled batch range means for this lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MultiProofMode {
    /// The MultiProof is required on L1: a range settles only once the
    /// Airbender range SNARK and the aggregated ZiSK range proof compose, so
    /// settlement consumes the range's ZiSK state.
    Required,
    /// Shadow proving: the Airbender-only proof settles the range, and the
    /// server never sends the MultiProof to L1. A settled range keeps its place
    /// in the ZiSK lane, so a late proof is still verified, measured, and
    /// logged.
    Shadow,
}

/// Manages ZiSK SNARK proof jobs with pick/submit assignment model.
pub struct ZiskJobManager {
    state: Mutex<ZiskJobState>,
    /// Assignment timeout — if a prover doesn't submit within this, the job is reassigned.
    assignment_timeout: Duration,
    /// When set, a commitment mismatch halts the node through the critical
    /// task listening on this channel (a mismatch means one proof system is
    /// wrong — a security event). Unset: log + count + retry.
    halt_on_mismatch: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<String>>>,
    /// Expected ZiSK verification keys per protocol version. A batch's set is
    /// looked up by `batch_metadata.batch_info.protocol_version`, so an upgrade
    /// window where two protocol versions coexist validates each batch against
    /// its own guest build — mirroring how the Airbender lane picks its VK per
    /// batch. No entry for a batch's version: the reported VKs are only logged
    /// (`zisk_lane_vk_drift`/`zisk_lane_vadcop_vk_drift` never fire). An entry:
    /// both VKs are drift-checked and a mismatch is rejected and counted. An
    /// empty map is the "no expected VK" (log-only) mode.
    expected_vks: HashMap<ProtocolSemanticVersion, ZiskVkSet>,
    /// When set, the lane runs in AGGREGATED mode: per-batch submissions
    /// are `vadcop_final` streams, every accepted one is buffered in the
    /// aggregation manager as range input, and discards are forwarded so
    /// broken ranges are dropped. See `zisk_aggregation_job_manager.rs`.
    aggregation_sink: std::sync::Mutex<
        Option<std::sync::Arc<crate::aggregation_job_manager::ZiskAggregationJobManager>>,
    >,
    /// Chain id + chain config: preimage of the `chain_config_hash` word in
    /// the guest's batch public input, needed to compute the expected value.
    chain_id: u64,
    chain_config: ZiskChainConfig,
    /// Whether to run the off-chain proof verification on each submission.
    /// True (the production default) runs the native verifier: the per-batch
    /// PLONK wire/binding check, and the full vadcop_final STARK check on the
    /// aggregated lane's stream. False skips only that cryptographic check; the
    /// batch commitment binding above it always runs.
    proof_verification_enabled: bool,
    /// What a settled batch range means for this lane. See
    /// [`Self::on_batches_settled`].
    multi_proof_mode: MultiProofMode,
}

impl ZiskJobManager {
    pub fn new(
        assignment_timeout: Duration,
        expected_vks: HashMap<ProtocolSemanticVersion, ZiskVkSet>,
        chain_id: u64,
        chain_config: ZiskChainConfig,
        proof_verification_enabled: bool,
        multi_proof_mode: MultiProofMode,
    ) -> Self {
        Self {
            state: Mutex::new(ZiskJobState {
                pending: HashMap::new(),
                assigned: HashMap::new(),
                completed: HashMap::new(),
                mismatch_attempts: HashMap::new(),
                backlog: HashMap::new(),
            }),
            assignment_timeout,
            halt_on_mismatch: std::sync::Mutex::new(None),
            expected_vks,
            aggregation_sink: std::sync::Mutex::new(None),
            chain_id,
            chain_config,
            proof_verification_enabled,
            multi_proof_mode,
        }
    }

    /// Switch the lane to aggregated mode: per-batch submissions become
    /// `vadcop_final` streams, accepted ones are buffered in `sink` as
    /// range input, and discards are forwarded to it.
    pub fn set_aggregation_sink(
        &self,
        sink: std::sync::Arc<crate::aggregation_job_manager::ZiskAggregationJobManager>,
    ) {
        *self.aggregation_sink.lock().expect("aggregation sink lock") = Some(sink);
    }

    fn aggregation_sink(
        &self,
    ) -> Option<std::sync::Arc<crate::aggregation_job_manager::ZiskAggregationJobManager>> {
        self.aggregation_sink
            .lock()
            .expect("aggregation sink lock")
            .clone()
    }

    /// Refresh the queue-depth/age gauges. Called under the state lock after
    /// every mutation, and periodically so ages advance while idle.
    fn record_queue_gauges(state: &ZiskJobState) {
        ZISK_LANE_METRICS
            .jobs_pending
            .set(state.pending.len() as u64);
        ZISK_LANE_METRICS
            .jobs_assigned
            .set(state.assigned.len() as u64);
        ZISK_LANE_METRICS
            .proofs_awaiting_snark
            .set(state.completed.len() as u64);
        ZISK_LANE_METRICS
            .backlog_entries
            .set(state.backlog.len() as u64);
        let oldest_age = state
            .pending
            .values()
            .map(|d| d.added_at)
            .chain(state.assigned.values().map(|(_, _, d)| d.added_at))
            .min()
            .map(|t| t.elapsed().as_secs())
            .unwrap_or(0);
        ZISK_LANE_METRICS.oldest_job_age_seconds.set(oldest_age);
    }

    /// Refresh the gauges without mutating the queue (periodic liveness).
    pub async fn refresh_gauges(&self) {
        Self::record_queue_gauges(&*self.state.lock().await);
    }

    /// Park a sealed batch's input because the active queue is full, instead of
    /// dropping it. Bounded: evicts expired and overflow entries so a lagging
    /// or offline Airbender lane cannot grow memory without bound.
    fn park_in_backlog(state: &mut ZiskJobState, batch_number: u64, job_data: ZiskJobData) {
        tracing::warn!(
            batch = batch_number,
            pending = state.pending.len(),
            assigned = state.assigned.len(),
            completed = state.completed.len(),
            max = MAX_TOTAL_JOBS,
            "ZiSK active queue full — parking the input in the backlog (provers offline or \
             Airbender lane behind); it is promoted when a slot frees"
        );
        state.backlog.insert(batch_number, job_data);
        // Lazy eviction: only scan when over capacity, so the common park is
        // O(1). This mirrors the previous data cache.
        if state.backlog.len() > MAX_BACKLOG_ENTRIES {
            Self::evict_backlog(state);
        }
    }

    /// Evict parked inputs that expired (older than `MAX_BACKLOG_AGE`) or that
    /// overflow `MAX_BACKLOG_ENTRIES` (oldest first). Every eviction is a sealed
    /// batch that loses its ZiSK proof path without a re-seal, so each raises
    /// the coverage-lost alarm as well as its by-reason counter.
    fn evict_backlog(state: &mut ZiskJobState) {
        let expired: Vec<u64> = state
            .backlog
            .iter()
            .filter(|(_, data)| data.added_at.elapsed() >= MAX_BACKLOG_AGE)
            .map(|(&batch, _)| batch)
            .collect();
        for batch in expired {
            state.backlog.remove(&batch);
            ZISK_LANE_METRICS.backlog_evictions[&ZiskBacklogEvictionReason::Expired].inc();
            ZISK_LANE_METRICS.coverage_lost.inc();
            tracing::error!(
                batch,
                "ZiSK coverage lost: evicting an expired sealed input from the backlog — \
                 the batch can no longer be proven without a re-seal"
            );
        }
        while state.backlog.len() > MAX_BACKLOG_ENTRIES {
            let Some((&oldest, _)) = state.backlog.iter().min_by_key(|(_, data)| data.added_at)
            else {
                break;
            };
            state.backlog.remove(&oldest);
            ZISK_LANE_METRICS.backlog_evictions[&ZiskBacklogEvictionReason::Overflow].inc();
            ZISK_LANE_METRICS.coverage_lost.inc();
            tracing::error!(
                batch = oldest,
                max = MAX_BACKLOG_ENTRIES,
                "ZiSK coverage lost: evicting a sealed input from the full backlog — \
                 the batch can no longer be proven without a re-seal"
            );
        }
    }

    /// Promote parked inputs into the active `pending` queue while it has room,
    /// lowest batch number first. Batch order is proving order and the
    /// SNARK/aggregation range order, so a required-mode range fills from its
    /// low end. Called whenever an active slot frees. Expired parked inputs are
    /// evicted first, so a stale input whose range is long gone is never
    /// promoted.
    fn promote_from_backlog(state: &mut ZiskJobState) {
        Self::evict_backlog(state);
        while state.total() < MAX_TOTAL_JOBS {
            let Some(&batch_number) = state.backlog.keys().min() else {
                break;
            };
            let job_data = state
                .backlog
                .remove(&batch_number)
                .expect("backlog key just observed");
            tracing::info!(
                batch = batch_number,
                "promoting parked ZiSK input into an active job"
            );
            state.pending.insert(batch_number, job_data);
        }
    }

    /// Arm halt-on-mismatch: a commitment mismatch will fire this sender,
    /// bringing the node down via the critical task that awaits it.
    pub fn set_halt_on_mismatch(&self, sender: tokio::sync::oneshot::Sender<String>) {
        *self.halt_on_mismatch.lock().expect("halt sender lock") = Some(sender);
    }

    /// How much work the lane holds, by lifecycle stage.
    pub async fn queue_counts(&self) -> ZiskQueueCounts {
        let state = self.state.lock().await;
        ZiskQueueCounts {
            jobs_pending: state.pending.len() as u64,
            jobs_assigned: state.assigned.len() as u64,
            proofs_completed: state.completed.len() as u64,
            inputs_in_backlog: state.backlog.len() as u64,
        }
    }

    /// Where a batch currently is in the ZiSK lane.
    pub async fn batch_status(&self, batch_number: u64) -> ZiskBatchStatus {
        let state = self.state.lock().await;
        if state.completed.contains_key(&batch_number) {
            ZiskBatchStatus::Completed
        } else if state.pending.contains_key(&batch_number)
            || state.assigned.contains_key(&batch_number)
        {
            ZiskBatchStatus::InFlight
        } else {
            ZiskBatchStatus::Unknown
        }
    }

    /// Add a batch ready for ZiSK proving. Called by the batcher at seal, so it
    /// never blocks the sequencer and never drops a sealed batch's input:
    /// - already pending, assigned, completed, or parked → left untouched;
    /// - room in the active queue → activated as a `pending` job;
    /// - active queue full → parked in the bounded backlog, to be promoted when
    ///   a slot frees.
    pub async fn add_job(&self, batch_number: u64, job_data: ZiskJobData) {
        let mut state = self.state.lock().await;
        if state.knows(batch_number) || state.backlog.contains_key(&batch_number) {
            tracing::debug!(batch = batch_number, "ZiSK job already known, skipping add");
            return;
        }
        if state.total() >= MAX_TOTAL_JOBS {
            Self::park_in_backlog(&mut state, batch_number, job_data);
            Self::record_queue_gauges(&state);
            return;
        }

        tracing::info!(
            batch = batch_number,
            zisk_data_bytes = job_data.zisk_data.len(),
            "ZiSK job added"
        );
        state.pending.insert(batch_number, job_data);
        Self::record_queue_gauges(&state);
    }

    /// The sealed ZiSK input bytes for a batch, if the manager still holds
    /// them — whether the job is active (pending or assigned) or parked in the
    /// backlog. Read-only; backs the debug `/ZiSK/{batch}/peek` endpoint.
    /// Returns `None` once a proof has been submitted (the input is replaced by
    /// the proof) or the batch has been discarded.
    pub async fn peek_input(&self, batch_number: u64) -> Option<Vec<u8>> {
        let state = self.state.lock().await;
        state
            .pending
            .get(&batch_number)
            .map(|data| data.zisk_data.clone())
            .or_else(|| {
                state
                    .assigned
                    .get(&batch_number)
                    .map(|(_, _, data)| data.zisk_data.clone())
            })
            .or_else(|| {
                state
                    .backlog
                    .get(&batch_number)
                    .map(|data| data.zisk_data.clone())
            })
    }

    /// Pick the next available ZiSK job for a prover.
    pub async fn pick_next_job(&self, prover_id: &str) -> Option<ZiskJob> {
        let now = std::time::Instant::now();
        let mut state = self.state.lock().await;

        // Return timed-out assigned jobs to pending.
        let timed_out: Vec<u64> = state
            .assigned
            .iter()
            .filter(|(_, (_, assigned_at, _))| {
                now.duration_since(*assigned_at) >= self.assignment_timeout
            })
            .map(|(&batch, _)| batch)
            .collect();
        for batch in timed_out {
            if let Some((old_prover, _, data)) = state.assigned.remove(&batch) {
                tracing::warn!(
                    batch,
                    old_prover,
                    "ZiSK job timed out, returning to pending"
                );
                state.pending.insert(batch, data);
            }
        }

        // Pick oldest pending.
        let batch_number = *state.pending.keys().min()?;
        let job_data = state.pending.remove(&batch_number)?;

        // The hash is already `0x`-prefixed. A second prefix would break the
        // daemon's `--supported-vk` filter, which strips exactly one.
        let vk_hash = job_data
            .batch_metadata
            .verification_key_hash()
            .map(str::to_string)
            .unwrap_or_else(|_| {
                tracing::warn!(batch = batch_number, "VK hash missing");
                String::new()
            });

        let zisk_data = job_data.zisk_data.clone();
        state
            .assigned
            .insert(batch_number, (prover_id.to_string(), now, job_data));
        Self::record_queue_gauges(&state);

        tracing::info!(batch = batch_number, prover_id, "ZiSK job assigned");

        Some(ZiskJob {
            batch_number,
            vk_hash,
            zisk_data,
        })
    }

    /// Submit a per-batch ZiSK proof: the raw `vadcop_final` stream, with
    /// empty `public_values` (the stream carries its publics).
    ///
    /// Validates the shape, the program-VK tripwire, and the batch
    /// commitment against the metadata captured at job creation, buffers
    /// the stream in the aggregation manager as range input, and parks a
    /// copy in the `completed` map.
    pub async fn submit_proof(
        &self,
        batch_number: u64,
        proof: Vec<u8>,
        public_values: Vec<u8>,
        prover_id: &str,
    ) -> Result<(), ZiskSubmitError> {
        // The server wires the sink at startup; a submission before that is
        // a wiring error, never a different proving mode.
        let Some(sink_ref) = self.aggregation_sink() else {
            return Err(ZiskSubmitError::AggregationNotWired);
        };
        let sink = Some(sink_ref);

        let (reported_vk, reported_vadcop_vk, commitment, vadcop_publics) = {
            if proof.len() != ZISK_VADCOP_STREAM_BYTES {
                return Err(ZiskSubmitError::InvalidProofSize {
                    got: proof.len(),
                    expected: ZISK_VADCOP_STREAM_BYTES,
                    hint: " (aggregated mode expects the raw vadcop_final stream; \
                           run the daemon with --aggregation)",
                });
            }
            if !public_values.is_empty() {
                return Err(ZiskSubmitError::InvalidPublicValuesSize {
                    got: public_values.len(),
                    expected: 0,
                });
            }
            let parsed =
                parse_vadcop_final_stream(&proof).map_err(ZiskSubmitError::MalformedProof)?;
            (
                parsed.program_vk,
                parsed.vadcop_vk,
                parsed.commitment,
                Some(parsed),
            )
        };

        // Select the expected VK set by the batch's protocol version, read from
        // the assigned job WITHOUT consuming it — a VK drift must leave the job
        // assigned so it times out back to pending for another prover. A job
        // that is not assigned has no version to key on; it falls through to the
        // `UnknownJob` error at the removal step below. The lookup is reused for
        // the off-chain verification's key binding after the job is removed:
        // reaching that point means the peek found the job, so the set matches.
        let expected_vks = {
            let state = self.state.lock().await;
            state
                .assigned
                .get(&batch_number)
                .and_then(|(_, _, data)| {
                    self.expected_vks
                        .get(&data.batch_metadata.batch_info.protocol_version)
                })
                .copied()
        };

        // Program VK tripwire: drift means the prover runs a different
        // guest build — reject before touching the job, so it stays assigned
        // and times out back to pending for another prover.
        if let Some(expected) = expected_vks.map(|set| set.program_vk) {
            if reported_vk != expected {
                ZISK_LANE_METRICS.vk_drift.inc();
                tracing::error!(
                    batch = batch_number,
                    prover_id,
                    %reported_vk,
                    %expected,
                    "ZiSK program VK drift — prover is running a different guest build"
                );
                return Err(ZiskSubmitError::VkDrift {
                    reported: reported_vk,
                    expected,
                });
            }
        } else {
            tracing::info!(batch = batch_number, %reported_vk, "ZiSK program VK reported (no expected VK configured for this protocol version)");
        }

        // Inner vadcop-final VK (rootCVadcopFinal) tripwire — same fail-closed
        // semantics as the program VK: reject before touching the job so it
        // times out back to pending.
        if let Some(expected) = expected_vks.map(|set| set.vadcop_vk)
            && reported_vadcop_vk != expected
        {
            ZISK_LANE_METRICS.vadcop_vk_drift.inc();
            tracing::error!(
                batch = batch_number,
                prover_id,
                reported_vadcop_vk = %reported_vadcop_vk,
                %expected,
                "ZiSK vadcop VK drift — prover is running a different recursive setup"
            );
            return Err(ZiskSubmitError::VadcopVkDrift {
                reported: reported_vadcop_vk,
                expected,
            });
        }

        // Remove from assigned jobs.
        let job_data = {
            let mut state = self.state.lock().await;
            let data = match state.assigned.remove(&batch_number) {
                Some((_, _, data)) => data,
                None => return Err(ZiskSubmitError::UnknownJob(batch_number)),
            };
            Self::record_queue_gauges(&state);
            data
        };

        // Validate the batch commitment against the metadata captured at
        // seal, using the guest lib's own hash functions.
        let stored = job_data.batch_metadata.batch_info.clone().into_stored();
        let prev = &job_data.batch_metadata.previous_stored_batch_info;
        let expected_commitment = crate::commitment::expected_zisk_public_input(
            &prev.state_commitment,
            &stored,
            self.chain_id,
            self.chain_config,
        );
        if commitment != expected_commitment {
            // The headline divergence alarm (one proof system is wrong):
            // always count + log; policy decides continue vs halt.
            let msg =
                format!("commitment mismatch: ZiSK={commitment}, expected={expected_commitment}");
            ZISK_LANE_METRICS.commitment_mismatches.inc();
            tracing::error!(batch = batch_number, "{msg}");
            if let Some(halt) = self
                .halt_on_mismatch
                .lock()
                .expect("halt sender lock")
                .take()
            {
                let _ = halt.send(format!(
                    "ZiSK commitment mismatch on batch {batch_number}: {msg}"
                ));
            } else {
                // Continue mode: retry a faulty/transient prover, but do NOT
                // requeue a DETERMINISTIC divergence forever — that leaks the
                // job's `MAX_TOTAL_JOBS` slot (`on_batches_settled` only
                // sweeps `completed`) until ZiSK coverage stops. Give up after
                // `MAX_COMMITMENT_MISMATCH_ATTEMPTS`: drop the job (freeing the
                // slot), raise the distinct `zisk_lane_unprovable` alert, and
                // stop requeuing. Sequencing is unaffected either way — the
                // primary Airbender lane never gates on ZiSK.
                let mut state = self.state.lock().await;
                let attempts = state.mismatch_attempts.entry(batch_number).or_insert(0);
                *attempts += 1;
                if *attempts >= MAX_COMMITMENT_MISMATCH_ATTEMPTS {
                    state.mismatch_attempts.remove(&batch_number);
                    // job_data intentionally dropped: not reinserted anywhere,
                    // so the slot is freed — let a parked input take it.
                    Self::promote_from_backlog(&mut state);
                    Self::record_queue_gauges(&state);
                    drop(state);
                    ZISK_LANE_METRICS.unprovable.inc();
                    tracing::error!(
                        batch = batch_number,
                        attempts = MAX_COMMITMENT_MISMATCH_ATTEMPTS,
                        "ZiSK lane unprovable: batch commitment mismatched on every attempt — \
                         giving up on this batch's ZiSK proof (job dropped, slot freed). One \
                         proof system disagrees deterministically; investigate. Sequencing is \
                         unaffected."
                    );
                } else {
                    state.pending.insert(batch_number, job_data);
                    Self::record_queue_gauges(&state);
                }
            }
            return Err(ZiskSubmitError::CommitmentMismatch);
        }

        // Off-chain proof verification (the second gate; the commitment binding
        // above is the first). The server verifies the `vadcop_final` STARK
        // stream natively BEFORE the proof is parked or composed — the real
        // cryptographic check of the STARK layer. The final BN254 PLONK
        // pairing of the range stays L1-verified. The
        // `proof_verification_enabled` toggle can skip this cryptographic
        // check; the commitment binding above always runs.
        let verification = if !self.proof_verification_enabled {
            Ok(())
        } else {
            zisk_verifier::verify_vadcop_final_stream(&proof)
        };
        if let Err(e) = verification {
            ZISK_LANE_METRICS.proof_verification_failures.inc();
            tracing::error!(
                batch = batch_number,
                prover_id,
                "ZiSK proof rejected by off-chain verification: {e}"
            );
            // Reject: do not park it, do not feed the aggregation sink. The job
            // was already removed from `assigned`, so it is dropped and its slot
            // is freed — let a parked input take it. The ZiSK lane never gates
            // sequencing.
            let mut state = self.state.lock().await;
            Self::promote_from_backlog(&mut state);
            Self::record_queue_gauges(&state);
            return Err(ZiskSubmitError::ProofVerificationFailed(e.to_string()));
        }

        tracing::info!(
            batch = batch_number,
            prover_id,
            zisk_proof_bytes = proof.len(),
            aggregated = vadcop_publics.is_some(),
            "ZiSK proof accepted"
        );

        // Aggregated mode: buffer a copy as range input for the aggregation
        // manager. When the input is not buffered do NOT park a completion
        // marker — a marker for an absent input would strand the range. The two
        // not-buffered cases differ, so they are handled apart (see
        // `AggregationInputOutcome`): a `BufferFull` input is re-parked so it is
        // retried once buffer space frees, while a `BelowFloor` input (its range
        // already went downstream) is dropped.
        if let (Some(sink), Some(parsed)) = (&sink, &vadcop_publics) {
            let outcome = sink
                .on_proof_completed(
                    batch_number,
                    AggregationInput {
                        stream: proof.clone(),
                        protocol_version: job_data
                            .batch_metadata
                            .batch_info
                            .protocol_version
                            .clone(),
                        program_vk: parsed.program_vk,
                        vadcop_vk: parsed.vadcop_vk,
                        commitment: parsed.commitment,
                    },
                )
                .await;
            match outcome {
                crate::aggregation_job_manager::AggregationInputOutcome::Buffered => {}
                crate::aggregation_job_manager::AggregationInputOutcome::BufferFull => {
                    // Keep the input in-process so it can prove once the buffer
                    // drains. Re-park rather than promote now: the buffer is
                    // still full, so re-activating immediately would only bounce
                    // straight back here.
                    let mut state = self.state.lock().await;
                    state.mismatch_attempts.remove(&batch_number);
                    Self::park_in_backlog(&mut state, batch_number, job_data);
                    Self::record_queue_gauges(&state);
                    tracing::warn!(
                        batch = batch_number,
                        "ZiSK aggregation buffer full — re-parked the input for retry"
                    );
                    return Ok(());
                }
                crate::aggregation_job_manager::AggregationInputOutcome::BelowFloor => {
                    // The range already went downstream; drop the input (the
                    // sink counted the lost coverage). The slot the job held is
                    // now free — let a parked input take it.
                    let mut state = self.state.lock().await;
                    state.mismatch_attempts.remove(&batch_number);
                    Self::promote_from_backlog(&mut state);
                    Self::record_queue_gauges(&state);
                    return Ok(());
                }
            }
        }

        {
            let mut state = self.state.lock().await;
            // Accepted: clear any prior mismatch attempts (a transient/faulty
            // prover recovered) so the give-up counter never carries over.
            state.mismatch_attempts.remove(&batch_number);
            state.completed.insert(batch_number, proof);
            Self::record_queue_gauges(&state);
        }

        ZISK_LANE_METRICS
            .time_to_submit
            .observe(job_data.added_at.elapsed());
        Ok(())
    }

    /// The batches at or below `batch_to` went downstream. Called from the
    /// Airbender SNARK submission path once the range is consumed; what the
    /// ZiSK lane keeps depends on the mode:
    ///
    /// - [`MultiProofMode::Required`]: the range settled through a composed
    ///   multi-proof (or, on the Airbender-only fallback, without one). Nothing
    ///   below the cut can compose anymore, so parked proofs AND parked inputs
    ///   are dropped here and the sink drops their aggregation state.
    /// - [`MultiProofMode::Shadow`]: settlement never waited for this lane, so a
    ///   settled batch keeps its proving path — a parked input still becomes a
    ///   job, still proves, and its range is still verified (late). Only the
    ///   parked proofs are swept: they are completion markers whose stream the
    ///   aggregation sink already holds, and sweeping them frees active slots
    ///   for the batches still to prove.
    ///
    /// In-flight jobs are left alone in both modes — their submit-time
    /// validation is the divergence signal.
    pub async fn on_batches_settled(&self, batch_to: u64) {
        if let Some(sink) = self.aggregation_sink() {
            sink.on_batches_settled(batch_to).await;
        }
        let mut state = self.state.lock().await;
        let stale_completed: Vec<u64> = state
            .completed
            .keys()
            .copied()
            .filter(|&b| b <= batch_to)
            .collect();
        let stale_backlog: Vec<u64> = match self.multi_proof_mode {
            // A parked input at or below a composed batch can never
            // prove-and-compose. Drop it, or the backlog leaks these entries.
            MultiProofMode::Required => state
                .backlog
                .keys()
                .copied()
                .filter(|&b| b <= batch_to)
                .collect(),
            MultiProofMode::Shadow => Vec::new(),
        };
        if stale_completed.is_empty() && stale_backlog.is_empty() {
            return;
        }
        for batch in &stale_completed {
            state.completed.remove(batch);
        }
        for batch in &stale_backlog {
            state.backlog.remove(batch);
        }
        // Freed active slots (swept `completed`) let higher parked inputs go
        // active — this replaces the old SNARK-arrival job re-creation.
        Self::promote_from_backlog(&mut state);
        tracing::info!(
            batch_to,
            discarded_completed = stale_completed.len(),
            discarded_backlog = stale_backlog.len(),
            "swept parked ZiSK proofs for batches already sent downstream"
        );
        Self::record_queue_gauges(&state);
    }

    /// Drop all ZiSK state for a batch range. Used by the fake-SNARK pass so
    /// batches consumed without a real Airbender SNARK (fake-prover
    /// environments, pre-V6 replay) don't leave orphaned jobs behind.
    pub async fn discard_batches(&self, batch_from: u64, batch_to: u64) {
        // The fake-SNARK pass consumes the lowest in-flight batches, so the
        // aggregation lane treats this as an up-to cut as well.
        if let Some(sink) = self.aggregation_sink() {
            sink.discard_up_to(batch_to).await;
        }
        let mut state = self.state.lock().await;
        let mut discarded = 0usize;
        for batch in batch_from..=batch_to {
            discarded += usize::from(state.pending.remove(&batch).is_some());
            discarded += usize::from(state.assigned.remove(&batch).is_some());
            discarded += usize::from(state.completed.remove(&batch).is_some());
            discarded += usize::from(state.backlog.remove(&batch).is_some());
            state.mismatch_attempts.remove(&batch);
        }
        if discarded > 0 {
            // Freed active slots let parked inputs outside the range go active.
            Self::promote_from_backlog(&mut state);
            tracing::debug!(batch_from, batch_to, discarded, "discarded ZiSK lane state");
            Self::record_queue_gauges(&state);
        }
    }

    /// Check if there are pending or assigned ZiSK jobs.
    pub async fn has_pending_jobs(&self) -> bool {
        let state = self.state.lock().await;
        !state.pending.is_empty() || !state.assigned.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commitment::ZISK_PUBLIC_VALUES_BYTES;
    use crate::test_util::create_test_batch_envelope;
    use crate::vadcop_stream::synthetic_stream;
    use zksync_os_batch_types::batcher_model::FriProof;
    use zksync_os_batch_types::batcher_model::ZISK_SNARK_PROOF_BYTES;
    use zksync_os_types::ProvingVersion;

    const TEST_PROTOCOL_VERSION: ProtocolSemanticVersion = ProtocolSemanticVersion::new(0, 31, 0);

    fn job_data(batch_number: u64, zisk_data: Vec<u8>) -> ZiskJobData {
        job_data_versioned(batch_number, zisk_data, TEST_PROTOCOL_VERSION)
    }

    /// Build job data whose batch carries `protocol_version`, so the
    /// version-keyed VK lookup in `submit_proof` can be exercised. The
    /// fixture's legacy genesis version has no batch-commitment encoding;
    /// `submit_proof` calls `into_stored`, which needs a current one (v30/v31).
    fn job_data_versioned(
        batch_number: u64,
        zisk_data: Vec<u8>,
        protocol_version: ProtocolSemanticVersion,
    ) -> ZiskJobData {
        let mut envelope = create_test_batch_envelope(batch_number, FriProof::Fake);
        envelope.batch.batch_info.protocol_version = protocol_version;
        ZiskJobData {
            zisk_data,
            batch_metadata: envelope.batch,
            added_at: std::time::Instant::now(),
        }
    }

    const TEST_CHAIN_ID: u64 = 270;
    const TEST_CHAIN_CONFIG: ZiskChainConfig = ZiskChainConfig {
        fri_proof_verification_enabled: false,
        max_tx_gas_limit: 1 << 24,
    };

    fn manager(expected_vk: Option<B256>) -> ZiskJobManager {
        // A configured program VK arms the drift tripwire for the fixture's v31
        // batches; the vadcop VK is pinned to zero so it matches the zeroed
        // `public_values[288..320]` the plain fixtures carry, exercising the
        // program VK alone. The per-batch PLONK lane submits well-shaped SNARK
        // artifacts, which pass wire-form verification, so proof verification
        // stays on here.
        let expected_vks = expected_vk
            .map(|program_vk| {
                HashMap::from([(
                    TEST_PROTOCOL_VERSION,
                    ZiskVkSet {
                        program_vk,
                        vadcop_vk: B256::ZERO,
                    },
                )])
            })
            .unwrap_or_default();
        ZiskJobManager::new(
            Duration::from_secs(60),
            expected_vks,
            TEST_CHAIN_ID,
            TEST_CHAIN_CONFIG,
            true,
            MultiProofMode::Required,
        )
    }

    /// A manager for the aggregated-lane tests. These tests submit synthetic
    /// `vadcop_final` streams that are structurally valid but are not real
    /// STARK proofs, so proof verification is disabled here; the batch
    /// commitment binding still runs. The real STARK verification has its own
    /// coverage in the `zisk-verifier` crate's fixture test.
    /// A manager with a wired aggregation sink, as the server runs it.
    /// Returns the sink so tests can inspect buffered inputs.
    fn manager_with_sink(
        expected_vk: Option<B256>,
        multi_proof_mode: MultiProofMode,
    ) -> (
        ZiskJobManager,
        std::sync::Arc<crate::aggregation_job_manager::ZiskAggregationJobManager>,
    ) {
        let expected_vks = expected_vk
            .map(|program_vk| {
                HashMap::from([(
                    TEST_PROTOCOL_VERSION,
                    ZiskVkSet {
                        program_vk,
                        // The stream fixtures carry these vadcop limbs.
                        vadcop_vk: vk_bytes([5, 6, 7, 8]),
                    },
                )])
            })
            .unwrap_or_default();
        let manager = ZiskJobManager::new(
            Duration::from_secs(60),
            expected_vks,
            TEST_CHAIN_ID,
            TEST_CHAIN_CONFIG,
            false,
            multi_proof_mode,
        );
        let agg = std::sync::Arc::new(
            crate::aggregation_job_manager::ZiskAggregationJobManager::new(
                1,
                Duration::from_secs(60),
                None,
                HashMap::new(),
                false,
                multi_proof_mode,
            ),
        );
        manager.set_aggregation_sink(agg.clone());
        (manager, agg)
    }

    /// The 32-byte wire form of four u64 VK limbs (big-endian each).
    fn vk_bytes(limbs: [u64; 4]) -> B256 {
        let mut out = [0u8; 32];
        for (word, chunk) in limbs.iter().zip(out.chunks_exact_mut(8)) {
            chunk.copy_from_slice(&word.to_be_bytes());
        }
        B256::from(out)
    }

    /// A well-formed stream whose commitment does NOT match any job's batch
    /// metadata, for mismatch tests.
    fn mismatching_vadcop_stream() -> Vec<u8> {
        synthetic_stream([1, 2, 3, 4], [5, 6, 7, 8], [0xFF; 32])
    }

    fn aggregated_lane_manager() -> ZiskJobManager {
        ZiskJobManager::new(
            Duration::from_secs(60),
            HashMap::new(),
            TEST_CHAIN_ID,
            TEST_CHAIN_CONFIG,
            false,
            MultiProofMode::Required,
        )
    }

    fn expected_commitment(data: &ZiskJobData) -> B256 {
        let stored = data.batch_metadata.batch_info.clone().into_stored();
        let prev = &data.batch_metadata.previous_stored_batch_info;
        crate::commitment::expected_zisk_public_input(
            &prev.state_commitment,
            &stored,
            TEST_CHAIN_ID,
            TEST_CHAIN_CONFIG,
        )
    }

    /// A `vadcop_final` stream whose commitment matches the job's batch
    /// metadata, for aggregated-mode submissions.
    fn matching_vadcop_stream(data: &ZiskJobData) -> Vec<u8> {
        synthetic_stream([1, 2, 3, 4], [5, 6, 7, 8], expected_commitment(data).0)
    }

    /// The seal-to-consumption happy path: an accepted stream parks in
    /// `completed` (status `Completed`) until the downstream send clears it
    /// via `on_batches_settled`, and the batch is `Unknown` afterwards.
    #[tokio::test]
    async fn accepted_proof_parks_until_discarded() {
        let (manager, _agg) = manager_with_sink(None, MultiProofMode::Required);

        let zisk_data = vec![0xAB; 32];
        let data = job_data(7, zisk_data.clone());
        let stream = matching_vadcop_stream(&data);
        manager.add_job(7, data).await;
        assert_eq!(manager.batch_status(7).await, ZiskBatchStatus::InFlight);

        let picked = manager
            .pick_next_job("prover-1")
            .await
            .expect("job available");
        assert_eq!(picked.batch_number, 7);
        assert_eq!(picked.zisk_data, zisk_data);
        assert_eq!(manager.batch_status(7).await, ZiskBatchStatus::InFlight);

        manager
            .submit_proof(7, stream, vec![], "prover-1")
            .await
            .expect("valid submission accepted");
        assert_eq!(manager.batch_status(7).await, ZiskBatchStatus::Completed);

        manager.on_batches_settled(7).await;
        assert_eq!(manager.batch_status(7).await, ZiskBatchStatus::Unknown);
    }

    /// `add_job` is idempotent across all three lifecycle maps: re-adding a
    /// batch that is pending, assigned, or completed leaves it untouched
    /// (the restart-regeneration path may re-offer batches).
    #[tokio::test]
    async fn add_job_is_idempotent() {
        let (manager, _agg) = manager_with_sink(None, MultiProofMode::Required);

        let data = job_data(7, vec![0xAB; 32]);
        let stream = matching_vadcop_stream(&data);
        manager.add_job(7, data).await;
        // Pending: re-add is a no-op.
        manager.add_job(7, job_data(7, vec![0xCD; 8])).await;
        let picked = manager
            .pick_next_job("prover-1")
            .await
            .expect("job available");
        assert_eq!(picked.zisk_data, vec![0xAB; 32], "original data kept");
        // Assigned: re-add is a no-op (no second pickable job appears).
        manager.add_job(7, job_data(7, vec![0xCD; 8])).await;
        assert!(manager.pick_next_job("prover-2").await.is_none());
        // Completed: re-add is a no-op (the parked proof is not clobbered).
        manager
            .submit_proof(7, stream, vec![], "prover-1")
            .await
            .expect("accepted");
        manager.add_job(7, job_data(7, vec![0xCD; 8])).await;
        assert_eq!(manager.batch_status(7).await, ZiskBatchStatus::Completed);
    }

    /// Pick-timeout reassignment. The assignment deadline is this crate's
    /// liveness mechanic: a prover that vanishes must not strand a batch, so
    /// the job returns to pending and the next prover receives the same
    /// prover input.
    #[tokio::test]
    async fn pick_timeout_reassigns_job() {
        // A zero timeout makes every assignment reassignable at the next pick.
        let manager = ZiskJobManager::new(
            Duration::ZERO,
            HashMap::new(),
            TEST_CHAIN_ID,
            TEST_CHAIN_CONFIG,
            true,
            MultiProofMode::Required,
        );
        manager.add_job(7, job_data(7, vec![0xAB; 32])).await;

        let first = manager
            .pick_next_job("prover-1")
            .await
            .expect("job available");
        assert_eq!(first.batch_number, 7);

        // prover-1 vanishes without submitting.
        let second = manager
            .pick_next_job("prover-2")
            .await
            .expect("job reassigned past the deadline");
        assert_eq!(second.batch_number, 7);
        assert_eq!(
            second.zisk_data, first.zisk_data,
            "the reassigned job carries the original prover input"
        );
    }

    /// Wire form of the picked job's VK hash. The daemon's `--supported-vk`
    /// filter strips exactly one `0x` prefix before it compares, so a doubled
    /// prefix makes every batch fail the filter and be skipped.
    #[tokio::test]
    async fn pick_reports_a_single_0x_prefixed_vk_hash() {
        let manager = manager(None);
        manager.add_job(7, job_data(7, vec![0xAB; 32])).await;

        let picked = manager
            .pick_next_job("prover-1")
            .await
            .expect("job available");
        assert_eq!(
            picked.vk_hash,
            ProvingVersion::V7.vk_hash(),
            "the pick reports the batch's VK hash verbatim"
        );
        assert!(picked.vk_hash.starts_with("0x"), "{}", picked.vk_hash);
        assert!(!picked.vk_hash.starts_with("0x0x"), "{}", picked.vk_hash);
        assert_eq!(picked.vk_hash.len(), 66, "{}", picked.vk_hash);
    }

    /// The fake-SNARK pass cleanup: discarding a range removes pending,
    /// assigned, and completed state so fake-prover environments don't
    /// accumulate orphaned jobs.
    #[tokio::test]
    async fn discard_batches_clears_all_state() {
        let (manager, _agg) = manager_with_sink(None, MultiProofMode::Required);

        let data8 = job_data(8, vec![2; 8]);
        let stream8 = matching_vadcop_stream(&data8);
        manager.add_job(7, job_data(7, vec![1; 8])).await;
        manager.add_job(8, data8).await;
        manager.add_job(9, job_data(9, vec![3; 8])).await;
        // 7 stays pending; 8 goes to completed; 9 assigned.
        // (pick order is by batch number: 7 first.)
        let picked = manager.pick_next_job("prover-1").await.expect("job");
        assert_eq!(picked.batch_number, 7);
        let picked = manager.pick_next_job("prover-1").await.expect("job");
        assert_eq!(picked.batch_number, 8);
        manager
            .submit_proof(8, stream8, vec![], "prover-1")
            .await
            .expect("accepted");

        manager.discard_batches(7, 9).await;
        for batch in 7..=9 {
            assert_eq!(manager.batch_status(batch).await, ZiskBatchStatus::Unknown);
        }
        assert!(!manager.has_pending_jobs().await);
    }

    /// With halt-on-mismatch armed, a commitment mismatch fires the halt
    /// channel (bringing the node down via the critical task) instead of
    /// silently requeuing the job for endless re-proving.
    #[tokio::test]
    async fn commitment_mismatch_fires_halt_when_armed() {
        let (manager, _agg) = manager_with_sink(None, MultiProofMode::Required);
        let (halt_tx, halt_rx) = tokio::sync::oneshot::channel();
        manager.set_halt_on_mismatch(halt_tx);

        manager.add_job(7, job_data(7, vec![0xAB; 32])).await;
        manager
            .pick_next_job("prover-1")
            .await
            .expect("job available");

        // A well-formed stream with a wrong commitment -> mismatch.
        let err = manager
            .submit_proof(7, mismatching_vadcop_stream(), vec![], "prover-1")
            .await
            .expect_err("mismatch must be rejected");
        assert!(matches!(err, ZiskSubmitError::CommitmentMismatch));

        let msg = halt_rx.await.expect("halt channel must fire");
        assert!(msg.contains("batch 7"), "{msg}");
        assert!(
            !manager.has_pending_jobs().await,
            "halting mode must not requeue the mismatching job"
        );
    }

    /// In continue mode a persistent (deterministic) commitment
    /// mismatch is given up on after `MAX_COMMITMENT_MISMATCH_ATTEMPTS`
    /// instead of requeuing forever — the job is dropped (slot freed), no
    /// state leaks, and the manager keeps serving other batches. Sequencing
    /// is unaffected because the ZiSK lane never gates it.
    #[tokio::test]
    async fn persistent_mismatch_gives_up_and_frees_slot() {
        // Continue mode: no halt armed.
        let (manager, _agg) = manager_with_sink(None, MultiProofMode::Required);
        manager.add_job(7, job_data(7, vec![0xAB; 32])).await;

        // Each attempt: pick the requeued job, submit a mismatching proof.
        for attempt in 1..=MAX_COMMITMENT_MISMATCH_ATTEMPTS {
            manager
                .pick_next_job("prover-1")
                .await
                .expect("job available for retry");
            let err = manager
                .submit_proof(7, mismatching_vadcop_stream(), vec![], "prover-1")
                .await
                .expect_err("mismatch must be rejected");
            assert!(matches!(err, ZiskSubmitError::CommitmentMismatch));
            if attempt < MAX_COMMITMENT_MISMATCH_ATTEMPTS {
                assert_eq!(
                    manager.batch_status(7).await,
                    ZiskBatchStatus::InFlight,
                    "requeued before the give-up threshold"
                );
            }
        }

        // Given up: no pending/assigned/completed state for the batch.
        assert_eq!(
            manager.batch_status(7).await,
            ZiskBatchStatus::Unknown,
            "an unprovable batch is dropped, not requeued"
        );
        assert!(
            !manager.has_pending_jobs().await,
            "the abandoned job must not leak a queue slot"
        );

        // The freed slot is reusable: a fresh batch is accepted and can prove.
        let data8 = job_data(8, vec![0xCD; 16]);
        let stream8 = matching_vadcop_stream(&data8);
        manager.add_job(8, data8).await;
        manager
            .pick_next_job("prover-1")
            .await
            .expect("job available");
        manager
            .submit_proof(8, stream8, vec![], "prover-1")
            .await
            .expect("a good proof for a later batch still lands");
        assert_eq!(manager.batch_status(8).await, ZiskBatchStatus::Completed);
    }

    /// A transient mismatch that later succeeds does not carry its attempt
    /// count forward: the give-up counter resets on acceptance.
    #[tokio::test]
    async fn mismatch_then_success_resets_attempts() {
        let (manager, _agg) = manager_with_sink(None, MultiProofMode::Required);
        let data = job_data(7, vec![0xAB; 32]);
        let good_stream = matching_vadcop_stream(&data);
        manager.add_job(7, data).await;

        // One mismatch (requeues), then a good proof lands.
        manager.pick_next_job("prover-1").await.expect("job");
        let _ = manager
            .submit_proof(7, mismatching_vadcop_stream(), vec![], "prover-1")
            .await
            .expect_err("mismatch rejected");
        manager.pick_next_job("prover-1").await.expect("re-picked");
        manager
            .submit_proof(7, good_stream, vec![], "prover-1")
            .await
            .expect("good proof accepted after a transient mismatch");
        assert_eq!(manager.batch_status(7).await, ZiskBatchStatus::Completed);
    }

    /// Aggregated mode: an accepted `vadcop_final` stream is buffered in
    /// the aggregation manager as range input AND parked here as a
    /// mode-tagged completion marker; the aggregation job carries the
    /// stream once its SNARK range is noted.
    #[tokio::test]
    async fn aggregated_mode_accepts_stream_and_feeds_sink() {
        use crate::aggregation_job_manager::ZiskAggregationJobManager;

        let manager = aggregated_lane_manager();
        let agg = std::sync::Arc::new(ZiskAggregationJobManager::new(
            1,
            Duration::from_secs(60),
            None,
            HashMap::new(),
            false,
            MultiProofMode::Required,
        ));
        manager.set_aggregation_sink(agg.clone());

        let data = job_data(7, vec![0xAB; 32]);
        let stream = matching_vadcop_stream(&data);
        manager.add_job(7, data).await;
        manager
            .pick_next_job("prover-1")
            .await
            .expect("job available");
        manager
            .submit_proof(7, stream.clone(), vec![], "prover-1")
            .await
            .expect("accepted");

        assert_eq!(manager.batch_status(7).await, ZiskBatchStatus::Completed);
        assert!(agg.has_input(7).await, "stream buffered as range input");

        agg.note_snark_range(7, 7).await;
        let job = agg
            .pick_next_job("agg-1")
            .await
            .expect("aggregation job formed");
        assert_eq!((job.from_batch, job.to_batch), (7, 7));
        assert_eq!(job.streams[0].1, stream);
    }

    /// Aggregated mode rejects PLONK-shaped submissions (and vice versa)
    /// with a size error, and rejects malformed streams before touching
    /// the job.
    #[tokio::test]
    async fn aggregated_mode_rejects_wrong_shapes() {
        use crate::aggregation_job_manager::ZiskAggregationJobManager;

        let manager = aggregated_lane_manager();
        let agg = std::sync::Arc::new(ZiskAggregationJobManager::new(
            1,
            Duration::from_secs(60),
            None,
            HashMap::new(),
            false,
            MultiProofMode::Required,
        ));
        manager.set_aggregation_sink(agg.clone());

        let data = job_data(7, vec![0xAB; 32]);
        let stream = matching_vadcop_stream(&data);
        manager.add_job(7, data).await;
        manager
            .pick_next_job("prover-1")
            .await
            .expect("job available");

        // A 768-byte PLONK proof is a mode mismatch.
        let err = manager
            .submit_proof(7, vec![0; ZISK_SNARK_PROOF_BYTES], vec![], "prover-1")
            .await
            .expect_err("plonk-sized proof rejected in aggregated mode");
        assert!(
            matches!(err, ZiskSubmitError::InvalidProofSize { .. }),
            "{err}"
        );
        assert!(err.to_string().contains("--aggregation"), "{err}");

        // Non-empty public values are a protocol error in aggregated mode.
        let err = manager
            .submit_proof(
                7,
                stream.clone(),
                vec![0; ZISK_PUBLIC_VALUES_BYTES],
                "prover-1",
            )
            .await
            .expect_err("non-empty publics rejected");
        assert!(matches!(
            err,
            ZiskSubmitError::InvalidPublicValuesSize { .. }
        ));

        // A right-sized but malformed stream (minimal flag) is rejected.
        let mut minimal = stream.clone();
        minimal[0] = 1;
        let err = manager
            .submit_proof(7, minimal, vec![], "prover-1")
            .await
            .expect_err("malformed stream rejected");
        assert!(matches!(err, ZiskSubmitError::MalformedProof(_)), "{err}");

        // The job survived all rejections: a valid submission still lands.
        manager
            .submit_proof(7, stream, vec![], "prover-1")
            .await
            .expect("valid stream accepted");
    }

    /// Discards forward to the aggregation sink so its buffered inputs and
    /// tracked ranges are dropped alongside the per-batch state.
    #[tokio::test]
    async fn discards_forward_to_aggregation_sink() {
        use crate::aggregation_job_manager::ZiskAggregationJobManager;

        let manager = aggregated_lane_manager();
        let agg = std::sync::Arc::new(ZiskAggregationJobManager::new(
            1,
            Duration::from_secs(60),
            None,
            HashMap::new(),
            false,
            MultiProofMode::Required,
        ));
        manager.set_aggregation_sink(agg.clone());

        let data = job_data(7, vec![0xAB; 32]);
        let stream = matching_vadcop_stream(&data);
        manager.add_job(7, data).await;
        manager
            .pick_next_job("prover-1")
            .await
            .expect("job available");
        manager
            .submit_proof(7, stream, vec![], "prover-1")
            .await
            .expect("accepted");
        agg.note_snark_range(7, 7).await;

        manager.discard_batches(7, 7).await;
        assert!(
            agg.pick_next_job("agg-1").await.is_none(),
            "discarded batch must not form an aggregation range"
        );
        assert!(!agg.has_input(7).await);
    }

    /// With an expected program VK configured, a stream that carries a
    /// different VK is rejected before the job is touched: the job stays
    /// assigned, and a corrected submission still succeeds.
    #[tokio::test]
    async fn vk_drift_rejects_submit_and_keeps_job_assigned() {
        // The good stream fixtures carry program limbs [1, 2, 3, 4].
        let (manager, _agg) =
            manager_with_sink(Some(vk_bytes([1, 2, 3, 4])), MultiProofMode::Required);

        let data = job_data(7, vec![0xAB; 32]);
        let commitment = expected_commitment(&data).0;
        manager.add_job(7, data).await;
        manager
            .pick_next_job("prover-1")
            .await
            .expect("job available");

        // Wrong program VK limbs -> drift rejection.
        let err = manager
            .submit_proof(
                7,
                synthetic_stream([9, 9, 9, 9], [5, 6, 7, 8], commitment),
                vec![],
                "prover-1",
            )
            .await
            .expect_err("VK drift must be rejected");
        assert!(matches!(err, ZiskSubmitError::VkDrift { .. }));

        // The job was not consumed or requeued: a stream with the expected
        // VK from the same assignment goes through.
        manager
            .submit_proof(
                7,
                synthetic_stream([1, 2, 3, 4], [5, 6, 7, 8], commitment),
                vec![],
                "prover-1",
            )
            .await
            .expect("corrected submission succeeds");
        assert_eq!(
            manager.batch_status(7).await,
            ZiskBatchStatus::Completed,
            "proof parked as the completion marker"
        );
    }

    /// With an expected inner vadcop VK configured, a stream that carries a
    /// different vadcop VK is rejected before the job is touched; a
    /// corrected submission still succeeds.
    #[tokio::test]
    async fn vadcop_vk_drift_rejects_submit_and_keeps_job_assigned() {
        // Program limbs match the fixtures; the vadcop expectation differs
        // from the [5, 6, 7, 8] the plain fixture carries.
        let expected_vadcop_limbs = [0x99u64, 0x99, 0x99, 0x99];
        let manager = ZiskJobManager::new(
            Duration::from_secs(60),
            HashMap::from([(
                TEST_PROTOCOL_VERSION,
                ZiskVkSet {
                    program_vk: vk_bytes([1, 2, 3, 4]),
                    vadcop_vk: vk_bytes(expected_vadcop_limbs),
                },
            )]),
            TEST_CHAIN_ID,
            TEST_CHAIN_CONFIG,
            false,
            MultiProofMode::Required,
        );
        manager.set_aggregation_sink(std::sync::Arc::new(
            crate::aggregation_job_manager::ZiskAggregationJobManager::new(
                1,
                Duration::from_secs(60),
                None,
                HashMap::new(),
                false,
                MultiProofMode::Required,
            ),
        ));

        let data = job_data(7, vec![0xAB; 32]);
        let commitment = expected_commitment(&data).0;
        manager.add_job(7, data).await;
        manager
            .pick_next_job("prover-1")
            .await
            .expect("job available");

        // The plain fixture vadcop limbs differ from the expectation -> drift.
        let err = manager
            .submit_proof(
                7,
                synthetic_stream([1, 2, 3, 4], [5, 6, 7, 8], commitment),
                vec![],
                "prover-1",
            )
            .await
            .expect_err("vadcop VK drift must be rejected");
        assert!(matches!(err, ZiskSubmitError::VadcopVkDrift { .. }));

        // Correct vadcop limbs from the same assignment -> accepted.
        manager
            .submit_proof(
                7,
                synthetic_stream([1, 2, 3, 4], expected_vadcop_limbs, commitment),
                vec![],
                "prover-1",
            )
            .await
            .expect("corrected submission succeeds");
        assert_eq!(manager.batch_status(7).await, ZiskBatchStatus::Completed);
    }

    /// The upgrade-window seam: two protocol versions with DIFFERENT
    /// configured program VKs each drift-check against their OWN VK (a batch
    /// proven under the other version's key is rejected), and a batch whose
    /// protocol version has no configured entry is accepted with the VK only
    /// logged. So two guest builds can be validated at once and adding a
    /// version is a config-only change.
    #[tokio::test]
    async fn vk_selected_per_protocol_version() {
        const V30: ProtocolSemanticVersion = ProtocolSemanticVersion::new(0, 30, 0);
        const V31: ProtocolSemanticVersion = ProtocolSemanticVersion::new(0, 31, 0);
        const V32: ProtocolSemanticVersion = ProtocolSemanticVersion::new(0, 32, 0);
        let limbs_v30 = [0x30u64, 0x30, 0x30, 0x30];
        let limbs_v31 = [0x31u64, 0x31, 0x31, 0x31];
        // The vadcop expectation matches the fixture limbs, so only the
        // program VK is exercised. V32 has no entry: its batches are
        // log-only.
        let manager = ZiskJobManager::new(
            Duration::from_secs(60),
            HashMap::from([
                (
                    V30,
                    ZiskVkSet {
                        program_vk: vk_bytes(limbs_v30),
                        vadcop_vk: vk_bytes([5, 6, 7, 8]),
                    },
                ),
                (
                    V31,
                    ZiskVkSet {
                        program_vk: vk_bytes(limbs_v31),
                        vadcop_vk: vk_bytes([5, 6, 7, 8]),
                    },
                ),
            ]),
            TEST_CHAIN_ID,
            TEST_CHAIN_CONFIG,
            false,
            MultiProofMode::Required,
        );
        let agg = std::sync::Arc::new(
            crate::aggregation_job_manager::ZiskAggregationJobManager::new(
                1,
                Duration::from_secs(60),
                None,
                HashMap::new(),
                false,
                MultiProofMode::Required,
            ),
        );
        manager.set_aggregation_sink(agg);

        // A v30 batch proven under the v30 key is accepted.
        let d30 = job_data_versioned(1, vec![0xAB; 8], V30);
        let s30 = synthetic_stream(limbs_v30, [5, 6, 7, 8], expected_commitment(&d30).0);
        manager.add_job(1, d30).await;
        manager.pick_next_job("p").await.expect("job 1");
        manager
            .submit_proof(1, s30, vec![], "p")
            .await
            .expect("v30 key accepted for a v30 batch");
        assert_eq!(manager.batch_status(1).await, ZiskBatchStatus::Completed);

        // A v31 batch is checked against ITS key (vk_v31): the v30 key drifts,
        // and the v31 key from the same assignment is then accepted.
        let d31 = job_data_versioned(2, vec![0xCD; 8], V31);
        let c31 = expected_commitment(&d31).0;
        manager.add_job(2, d31).await;
        manager.pick_next_job("p").await.expect("job 2");
        let err = manager
            .submit_proof(
                2,
                synthetic_stream(limbs_v30, [5, 6, 7, 8], c31),
                vec![],
                "p",
            )
            .await
            .expect_err("the other version's key must drift");
        assert!(matches!(err, ZiskSubmitError::VkDrift { .. }), "{err}");
        manager
            .submit_proof(
                2,
                synthetic_stream(limbs_v31, [5, 6, 7, 8], c31),
                vec![],
                "p",
            )
            .await
            .expect("v31 key accepted for a v31 batch");
        assert_eq!(manager.batch_status(2).await, ZiskBatchStatus::Completed);

        // A v32 batch has no configured entry: any VK is accepted (log-only).
        let d32 = job_data_versioned(3, vec![0xEE; 8], V32);
        let s32 = synthetic_stream(
            [0xAA, 0xAA, 0xAA, 0xAA],
            [5, 6, 7, 8],
            expected_commitment(&d32).0,
        );
        manager.add_job(3, d32).await;
        manager.pick_next_job("p").await.expect("job 3");
        manager
            .submit_proof(3, s32, vec![], "p")
            .await
            .expect("an unmapped protocol version is log-only, so accepted");
        assert_eq!(manager.batch_status(3).await, ZiskBatchStatus::Completed);
    }

    /// A full active queue parks new inputs instead of dropping them, then
    /// frees them back into the active queue lowest batch first as slots open.
    /// This is the in-process backpressure that replaced the out-of-band data
    /// cache and its SNARK-arrival re-creation fallback.
    #[tokio::test]
    async fn full_queue_parks_inputs_then_promotes_lowest_first() {
        let manager = manager(None);

        // Fill the active queue exactly to capacity.
        for batch in 1..=MAX_TOTAL_JOBS as u64 {
            manager
                .add_job(batch, job_data(batch, vec![batch as u8]))
                .await;
        }

        // Two more batches seal while the queue is full: parked, not dropped.
        let parked_low = MAX_TOTAL_JOBS as u64 + 1;
        let parked_high = MAX_TOTAL_JOBS as u64 + 2;
        manager
            .add_job(parked_low, job_data(parked_low, vec![0xAA]))
            .await;
        manager
            .add_job(parked_high, job_data(parked_high, vec![0xBB]))
            .await;

        // A parked input has no active job yet, but its bytes stay peekable.
        assert_eq!(
            manager.batch_status(parked_low).await,
            ZiskBatchStatus::Unknown
        );
        assert_eq!(manager.peek_input(parked_low).await, Some(vec![0xAA]));

        // Free one slot: the LOWEST parked batch is promoted to an active job;
        // the higher one waits for the next free slot.
        manager.discard_batches(1, 1).await;
        assert_eq!(
            manager.batch_status(parked_low).await,
            ZiskBatchStatus::InFlight,
            "lowest parked batch promoted into the active queue"
        );
        assert_eq!(
            manager.batch_status(parked_high).await,
            ZiskBatchStatus::Unknown,
            "the higher parked batch waits for the next free slot"
        );

        // Free another slot: the remaining parked batch is promoted.
        manager.discard_batches(2, 2).await;
        assert_eq!(
            manager.batch_status(parked_high).await,
            ZiskBatchStatus::InFlight
        );
    }

    /// Settlement passing a parked input must not void that batch's ZiSK
    /// coverage in shadow proving, where settlement never waited for this lane:
    /// the sealed input stays parked and still becomes a job. Under a required
    /// multi-proof, settlement means the range composed, so the same input is
    /// dropped.
    #[tokio::test]
    async fn shadow_mode_keeps_parked_inputs_past_settlement() {
        for (mode, kept) in [
            (MultiProofMode::Required, false),
            (MultiProofMode::Shadow, true),
        ] {
            let (manager, _agg) = manager_with_sink(None, mode);
            // Fill the active queue so the next sealed batch parks.
            for batch in 1..=MAX_TOTAL_JOBS as u64 {
                manager
                    .add_job(batch, job_data(batch, vec![batch as u8]))
                    .await;
            }
            let parked = MAX_TOTAL_JOBS as u64 + 1;
            manager.add_job(parked, job_data(parked, vec![0xAA])).await;
            assert_eq!(manager.peek_input(parked).await, Some(vec![0xAA]));

            manager.on_batches_settled(parked).await;
            assert_eq!(
                manager.peek_input(parked).await.is_some(),
                kept,
                "{mode:?}: the settled batch's parked input"
            );
        }
    }

    /// Every parked input the backlog bound forces out is ZiSK coverage the
    /// lane will never provide, so each eviction raises the coverage-lost
    /// alarm.
    #[tokio::test]
    async fn backlog_overflow_counts_lost_coverage() {
        let (manager, _agg) = manager_with_sink(None, MultiProofMode::Shadow);
        let lost_before = ZISK_LANE_METRICS.coverage_lost.get();

        // Fill the active queue and the backlog, then overflow the backlog.
        let overflow = 2u64;
        let sealed = (MAX_TOTAL_JOBS + MAX_BACKLOG_ENTRIES) as u64 + overflow;
        for batch in 1..=sealed {
            manager
                .add_job(batch, job_data(batch, vec![batch as u8]))
                .await;
        }

        assert_eq!(
            manager.queue_counts().await.inputs_in_backlog,
            MAX_BACKLOG_ENTRIES as u64,
            "the backlog stays at its bound"
        );
        assert_eq!(
            ZISK_LANE_METRICS.coverage_lost.get() - lost_before,
            overflow,
            "one alarm per evicted input"
        );
    }

    /// `peek_input` serves the sealed bytes while the job is active (pending or
    /// assigned) or parked, and stops once the batch leaves the manager.
    #[tokio::test]
    async fn peek_input_covers_active_and_parked() {
        let manager = manager(None);
        assert_eq!(manager.peek_input(7).await, None, "unknown batch");

        manager.add_job(7, job_data(7, vec![0xAB; 4])).await;
        assert_eq!(
            manager.peek_input(7).await,
            Some(vec![0xAB; 4]),
            "active (pending)"
        );

        manager.pick_next_job("prover-1").await.expect("job");
        assert_eq!(
            manager.peek_input(7).await,
            Some(vec![0xAB; 4]),
            "active (assigned)"
        );

        manager.discard_batches(7, 7).await;
        assert_eq!(manager.peek_input(7).await, None, "discarded");
    }
}
