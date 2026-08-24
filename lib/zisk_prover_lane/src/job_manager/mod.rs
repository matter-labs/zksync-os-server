//! Job manager for per-batch ZiSK proof generation, mirroring `FriJobManager`.
//!
//! A batch enters through `add_job` at seal, so ZiSK proving runs concurrently
//! with the Airbender lane rather than behind it. `add_job` never blocks the
//! batcher: a full active queue parks the input in a bounded backlog and
//! promotes it, lowest batch first, when a slot frees.
//!
//! External provers `pick_next_job` (with timeout-based reassignment) and
//! `submit_proof` the raw `vadcop_final` stream. An accepted submission is
//! checked for shape, guest-build keys, the STARK itself and the batch
//! commitment, then handed to the aggregation lane, which owns it from then
//! on — this lane keeps nothing about the batch afterwards. The final PLONK
//! pairing stays L1-verified.
//!
//! Nothing is sent downstream from here: the range proving stage composes the
//! multi-proof.

use crate::aggregation_job_manager::{AggregationInput, AggregationInputOutcome};
use crate::metrics::{ZISK_LANE_METRICS, ZiskBacklogEvictionReason};
use crate::proving_version::ZiskProvingVersion;
use crate::vadcop_stream::{ZISK_VADCOP_STREAM_BYTES, parse_vadcop_final_stream};
use alloy::primitives::B256;
use std::collections::{BTreeMap, HashMap};
use std::time::Duration;
use tokio::sync::Mutex;
use zksync_os_batch_types::batcher_model::BatchMetadata;
use zksync_os_types::ProtocolSemanticVersion;

/// Maximum number of active jobs — pending plus assigned. Prevents unbounded
/// memory growth if ZiSK provers are slow or offline. When full, `add_job`
/// parks the input in the backlog (below) instead of activating it. An accepted
/// proof frees its slot at once, since the aggregation lane owns the stream
/// from that point.
pub const MAX_TOTAL_JOBS: usize = 50;

/// How many STARK verifications may run at once. Small: each one saturates a
/// core for seconds, and the blocking pool is shared with the rest of the node.
const MAX_CONCURRENT_VERIFICATIONS: usize = 4;

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

/// What a commitment mismatch means once the local arbiter is consulted.
///
/// With `E` the commitment expected from the native lane, `S` the one in the
/// submission and `L` the one seal-time shadow execution produced, a mismatch
/// (`S != E`) is only evidence of a divergence between the proof systems when
/// an independent local run reproduces the submission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MismatchClassification {
    /// `L == S`: local re-execution reproduced the submission against the
    /// native result. Two independent ZiSK runs agree, so the disagreement is
    /// between the proof systems themselves.
    CorroboratedDivergence,
    /// `L == E`: local re-execution agrees with the native result, so the
    /// submission is simply wrong — the prover is at fault, not the system.
    WrongResult,
    /// No local answer, or three distinct values. Nothing can be concluded;
    /// never infer a divergence from an inconclusive check.
    Inconclusive,
}

/// Classify a commitment mismatch against the local arbiter. See
/// [`MismatchClassification`].
pub(crate) fn classify_mismatch(
    seal_shadow_commitment: Option<B256>,
    submitted: B256,
    expected: B256,
) -> MismatchClassification {
    match seal_shadow_commitment {
        Some(local) if local == submitted => MismatchClassification::CorroboratedDivergence,
        Some(local) if local == expected => MismatchClassification::WrongResult,
        _ => MismatchClassification::Inconclusive,
    }
}

/// Whether a mismatching submission justifies halting the node.
///
/// Stopping the node is the loudest action available, so it takes the
/// strongest evidence: a proof this server cryptographically verified, whose
/// result an independent local re-execution reproduced. Anything weaker is a
/// byte string a caller supplied — reachable by anyone who can call submit —
/// and must never be able to stop block production.
pub(crate) fn should_halt(proof_is_verified: bool, classification: MismatchClassification) -> bool {
    proof_is_verified && classification == MismatchClassification::CorroboratedDivergence
}

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
    /// The guest commitment computed by seal-time shadow execution (`L`).
    /// Submit-time classification needs an independent local answer to tell a
    /// prover's wrong result apart from a genuine divergence; without it
    /// (shadow execution off, or the guest failed) neither can be concluded.
    pub seal_shadow_commitment: Option<B256>,
}

/// Where a batch currently is in this lane. There is no accepted state: an
/// accepted proof belongs to the aggregation lane, and this lane forgets the
/// batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZiskBatchStatus {
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
    /// Submissions this lane has accepted and handed to the aggregation lane.
    /// Monotonic: nothing is parked here anymore, so this counts events, not
    /// held state.
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
    #[error("unknown batch {0}")]
    UnknownJob(u64),
    #[error("submission for batch {0} was superseded by a newer lease")]
    Superseded(u64),
    #[error("the per-batch proving stage is unavailable for batch {0}")]
    CompletionUnavailable(u64),
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
    #[error(
        "this server binary has no compiled ZiSK release manifest for protocol version {version}"
    )]
    MissingVersionKeys { version: ProtocolSemanticVersion },
    #[error("cannot derive the expected batch public input: {0}")]
    PublicInputUnderivable(String),
}

/// Everything the lane judges a commitment mismatch on: what the prover
/// submitted, what the batch metadata implies, whether the proof itself
/// verified, and the job it arrived for.
struct MismatchEvidence<'a> {
    batch_number: u64,
    generation: u64,
    prover_id: &'a str,
    commitment: B256,
    expected_commitment: B256,
    proof_is_verified: bool,
}

/// Where one batch is in this lane.
///
/// One value per batch, so a batch cannot be in two places at once and a retry
/// count cannot outlive the job it belongs to. `Backlogged` is the only state
/// that holds no active slot: it is the in-process backpressure buffer that
/// lets `add_job` stay non-blocking without dropping a sealed batch's input.
enum BatchJob {
    /// Sealed input parked because the active queue was full.
    Backlogged(ZiskJobData),
    /// Active and offerable to a prover.
    Pending { data: ZiskJobData, failures: u32 },
    /// Active and leased to a prover until it submits or times out.
    Assigned {
        data: ZiskJobData,
        lease: Lease,
        failures: u32,
    },
}

/// Who holds a job and since when.
struct Lease {
    prover_id: String,
    since: std::time::Instant,
    /// Unique for the lifetime of this manager. A submission captures it
    /// before its first expensive await and may only commit that same lease.
    generation: u64,
}

impl BatchJob {
    fn data(&self) -> &ZiskJobData {
        match self {
            Self::Backlogged(data) | Self::Pending { data, .. } | Self::Assigned { data, .. } => {
                data
            }
        }
    }

    /// Whether the job occupies one of the `MAX_TOTAL_JOBS` active slots.
    fn is_active(&self) -> bool {
        !matches!(self, Self::Backlogged(_))
    }
}

/// Inner state protected by a single mutex to avoid lock ordering issues.
struct ZiskJobState {
    /// One entry per batch this lane knows about, in batch order — which is
    /// proving order, and the order both the pick and the promotion policy
    /// follow.
    jobs: BTreeMap<u64, BatchJob>,
    /// Submissions accepted and handed on, since startup. Not held state —
    /// the per-batch lane keeps nothing after acceptance — but the operator
    /// status endpoint reports it as the lane's throughput signal.
    proofs_accepted: u64,
    /// Monotonic across every assignment in this process, including a batch
    /// that is discarded and later re-added by an in-process re-seal.
    next_generation: u64,
}

impl ZiskJobState {
    fn issue_generation(&mut self) -> u64 {
        let generation = self.next_generation;
        self.next_generation = self
            .next_generation
            .checked_add(1)
            .expect("ZiSK lease generation overflow");
        generation
    }

    /// Jobs holding an active slot. Parked inputs do not count.
    fn active(&self) -> usize {
        self.jobs.values().filter(|job| job.is_active()).count()
    }

    /// Whether the batch has an active job. A parked input is not yet one.
    fn knows(&self, batch_number: u64) -> bool {
        self.jobs
            .get(&batch_number)
            .is_some_and(BatchJob::is_active)
    }

    fn counts(&self) -> (usize, usize, usize) {
        let mut pending = 0;
        let mut assigned = 0;
        let mut backlogged = 0;
        for job in self.jobs.values() {
            match job {
                BatchJob::Backlogged(_) => backlogged += 1,
                BatchJob::Pending { .. } => pending += 1,
                BatchJob::Assigned { .. } => assigned += 1,
            }
        }
        (pending, assigned, backlogged)
    }

    fn batches_in<'a>(
        &'a self,
        is: impl Fn(&BatchJob) -> bool + 'a,
    ) -> impl Iterator<Item = u64> + 'a {
        self.jobs
            .iter()
            .filter(move |(_, job)| is(job))
            .map(|(&batch, _)| batch)
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

/// What the per-batch lane checks and how long it waits.
pub struct ZiskLaneConfig {
    /// If a prover does not submit within this, the job is reassigned.
    pub assignment_timeout: Duration,
    /// Expected ZiSK verification keys per protocol version.
    pub expected_vks: HashMap<ProtocolSemanticVersion, ZiskVkSet>,
    /// Needed for the `chain_config_hash` word of the v32 batch public input.
    pub chain_id: u64,
    /// Run the native verifier on each submission.
    pub proof_verification_enabled: bool,
}

/// Who the per-batch lane publishes to.
pub struct ZiskLaneWiring {
    /// Where accepted `vadcop_final` streams go, and where discards are
    /// forwarded so broken ranges are dropped.
    pub aggregation_sink: std::sync::Arc<crate::aggregation_job_manager::ZiskAggregationJobManager>,
    /// What losing a batch's proof costs, together with what that answer
    /// requires. The two cannot be set apart, so a `Required` lane with no way
    /// to release its gated batches is not a value anyone can build.
    pub mode: ZiskLaneMode,
    /// Fires once on a verified, locally corroborated commitment mismatch.
    /// `None` leaves the lane logging and retrying instead.
    pub halt_on_mismatch: Option<tokio::sync::oneshot::Sender<String>>,
}

/// The lane's mode and everything that mode needs.
pub enum ZiskLaneMode {
    /// Settlement never waits for this lane, so a lost proof costs coverage and
    /// nothing announces completions.
    Shadow,
    /// Settlement waits: a batch is held at the commit gate until this lane
    /// proves it, and `batch_ready` is the only thing that releases it.
    Required {
        batch_ready: tokio::sync::mpsc::Sender<u64>,
    },
}

impl ZiskLaneMode {
    fn multi_proof_mode(&self) -> MultiProofMode {
        match self {
            Self::Shadow => MultiProofMode::Shadow,
            Self::Required { .. } => MultiProofMode::Required,
        }
    }

    fn into_batch_ready(self) -> Option<tokio::sync::mpsc::Sender<u64>> {
        match self {
            Self::Shadow => None,
            Self::Required { batch_ready } => Some(batch_ready),
        }
    }
}

/// Manages ZiSK SNARK proof jobs with pick/submit assignment model.
pub struct ZiskJobManager {
    state: Mutex<ZiskJobState>,
    /// Assignment timeout — if a prover doesn't submit within this, the job is reassigned.
    assignment_timeout: Duration,
    /// Announces batches whose proof this lane has accepted, so the per-batch
    /// proving stage can release them without polling. Only the stage that
    /// gates on both proofs sets it.
    batch_ready: Option<tokio::sync::mpsc::Sender<u64>>,
    /// When set, a commitment mismatch halts the node through the critical
    /// task listening on this channel (a mismatch means one proof system is
    /// wrong — a security event). Unset: log + count + retry.
    halt_on_mismatch: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<String>>>,
    /// Expected ZiSK verification keys per protocol version. A batch's set is
    /// looked up by `batch_metadata.batch_info.protocol_version`, so an upgrade
    /// window where two protocol versions coexist validates each batch against
    /// its own guest build — mirroring how the Airbender lane picks its VK per
    /// batch. No entry for a batch's version: in `Shadow` the reported VKs are
    /// only logged, and in `Required` the submission is refused — a proof bound
    /// for L1 is never accepted against an unpinned guest build. An entry:
    /// both VKs are drift-checked and a mismatch is rejected and counted. An
    /// empty map means no version is pinned, which `Shadow` treats as log-only
    /// and `Required` refuses outright.
    expected_vks: HashMap<ProtocolSemanticVersion, ZiskVkSet>,
    /// Where accepted per-batch `vadcop_final` streams go: the aggregation
    /// manager buffers them as range input, and discards are forwarded so
    /// broken ranges are dropped. See `aggregation_job_manager.rs`.
    aggregation_sink: std::sync::Arc<crate::aggregation_job_manager::ZiskAggregationJobManager>,
    /// Needed for the `chain_config_hash` word of the v32 batch public input.
    chain_id: u64,
    /// Whether to run the off-chain proof verification on each submission.
    /// True (the production default) runs the native verifier: the per-batch
    /// PLONK wire/binding check, and the full vadcop_final STARK check on the
    /// aggregated lane's stream. False skips only that cryptographic check; the
    /// batch commitment binding above it always runs.
    proof_verification_enabled: bool,
    /// Bounds how many STARK verifications run at once. A verification is
    /// seconds of CPU on the blocking pool that RocksDB and the Airbender lane
    /// also use, and the submit endpoint is unauthenticated: nothing else caps
    /// how many a caller can start, since a job stays assigned across a
    /// rejected submission and can be submitted for again.
    verification_slots: tokio::sync::Semaphore,
    /// What a settled batch range means for this lane. See
    /// [`Self::on_batches_settled`].
    multi_proof_mode: MultiProofMode,
}

impl ZiskJobManager {
    /// Settings and collaborators are separate arguments because they answer
    /// different questions: what the lane checks, and who it publishes to. The
    /// collaborators arrive here rather than through setters — a manager
    /// without its aggregation sink could accept nothing, and one without
    /// `batch_ready` could never release a gated batch, so those states are now
    /// unconstructable rather than merely discouraged.
    pub fn new(config: ZiskLaneConfig, wiring: ZiskLaneWiring) -> Self {
        let ZiskLaneConfig {
            assignment_timeout,
            expected_vks,
            chain_id,
            proof_verification_enabled,
        } = config;
        let ZiskLaneWiring {
            aggregation_sink,
            mode,
            halt_on_mismatch,
        } = wiring;
        let multi_proof_mode = mode.multi_proof_mode();
        let batch_ready = mode.into_batch_ready();
        Self {
            state: Mutex::new(ZiskJobState {
                jobs: BTreeMap::new(),
                proofs_accepted: 0,
                next_generation: 0,
            }),
            assignment_timeout,
            batch_ready,
            halt_on_mismatch: std::sync::Mutex::new(halt_on_mismatch),
            expected_vks,
            aggregation_sink,
            chain_id,
            proof_verification_enabled,
            verification_slots: tokio::sync::Semaphore::new(MAX_CONCURRENT_VERIFICATIONS),
            multi_proof_mode,
        }
    }

    /// Refresh the queue-depth/age gauges. Called under the state lock after
    /// every mutation, and periodically so ages advance while idle.
    fn record_queue_gauges(state: &ZiskJobState) {
        let (pending, assigned, backlogged) = state.counts();
        ZISK_LANE_METRICS.jobs_pending.set(pending as u64);
        ZISK_LANE_METRICS.jobs_assigned.set(assigned as u64);
        ZISK_LANE_METRICS.backlog_entries.set(backlogged as u64);
        let oldest_age = state
            .jobs
            .values()
            .filter(|job| job.is_active())
            .map(|job| job.data().added_at)
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
    fn park_in_backlog(
        state: &mut ZiskJobState,
        batch_number: u64,
        job_data: ZiskJobData,
        mode: MultiProofMode,
    ) {
        let (pending, assigned, _) = state.counts();
        tracing::warn!(
            batch = batch_number,
            pending,
            assigned,
            max = MAX_TOTAL_JOBS,
            "ZiSK active queue full — parking the input in the backlog (provers offline or \
             Airbender lane behind); it is promoted when a slot frees"
        );
        state
            .jobs
            .insert(batch_number, BatchJob::Backlogged(job_data));
        // Lazy eviction: only scan when over capacity, so the common park is
        // O(1).
        if state.counts().2 > MAX_BACKLOG_ENTRIES {
            Self::evict_backlog(state, mode);
        }
    }

    /// Evict parked inputs that expired (older than `MAX_BACKLOG_AGE`) or that
    /// overflow `MAX_BACKLOG_ENTRIES` (oldest first). Every eviction is a sealed
    /// batch that loses its ZiSK proof path without a re-seal, so each raises
    /// the coverage-lost alarm as well as its by-reason counter.
    fn evict_backlog(state: &mut ZiskJobState, mode: MultiProofMode) {
        // In `Required` every parked input is indispensable: the batch is held
        // at the commit gate until this lane proves it, so evicting one turns a
        // recoverable stall into a permanent one that only a re-seal could
        // clear. Nothing needs evicting there either — the gate's admission
        // window bounds how many batches can be in this lane at once.
        if mode == MultiProofMode::Required {
            return;
        }
        let expired: Vec<u64> = state
            .jobs
            .iter()
            .filter(|(_, job)| {
                matches!(job, BatchJob::Backlogged(_))
                    && job.data().added_at.elapsed() >= MAX_BACKLOG_AGE
            })
            .map(|(&batch, _)| batch)
            .collect();
        for batch in expired {
            state.jobs.remove(&batch);
            ZISK_LANE_METRICS.backlog_evictions[&ZiskBacklogEvictionReason::Expired].inc();
            ZISK_LANE_METRICS.coverage_lost.inc();
            tracing::error!(
                batch,
                "ZiSK coverage lost: evicting an expired sealed input from the backlog — \
                 the batch can no longer be proven without a re-seal"
            );
        }
        while state.counts().2 > MAX_BACKLOG_ENTRIES {
            let Some(oldest) = state
                .jobs
                .iter()
                .filter(|(_, job)| matches!(job, BatchJob::Backlogged(_)))
                .min_by_key(|(_, job)| job.data().added_at)
                .map(|(&batch, _)| batch)
            else {
                break;
            };
            state.jobs.remove(&oldest);
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
    fn promote_from_backlog(state: &mut ZiskJobState, mode: MultiProofMode) {
        Self::evict_backlog(state, mode);
        while state.active() < MAX_TOTAL_JOBS {
            let Some(batch_number) = state
                .batches_in(|job| matches!(job, BatchJob::Backlogged(_)))
                .next()
            else {
                break;
            };
            let Some(BatchJob::Backlogged(data)) = state.jobs.remove(&batch_number) else {
                unreachable!("filtered to backlogged jobs");
            };
            state
                .jobs
                .insert(batch_number, BatchJob::Pending { data, failures: 0 });
            tracing::info!(
                batch = batch_number,
                "promoting parked ZiSK input into an active job"
            );
        }
    }

    /// How much work the lane holds, by lifecycle stage.
    pub async fn queue_counts(&self) -> ZiskQueueCounts {
        let state = self.state.lock().await;
        let (pending, assigned, backlogged) = state.counts();
        ZiskQueueCounts {
            jobs_pending: pending as u64,
            jobs_assigned: assigned as u64,
            proofs_completed: state.proofs_accepted,
            inputs_in_backlog: backlogged as u64,
        }
    }

    /// Where a batch currently is in the ZiSK lane.
    pub async fn batch_status(&self, batch_number: u64) -> ZiskBatchStatus {
        let state = self.state.lock().await;
        if state.knows(batch_number) {
            ZiskBatchStatus::InFlight
        } else {
            ZiskBatchStatus::Unknown
        }
    }

    /// Add a batch ready for ZiSK proving. Called by the batcher at seal, so it
    /// never blocks the sequencer and never drops a sealed batch's input:
    /// - already active or parked → left untouched;
    /// - room in the active queue → activated as a `pending` job;
    /// - active queue full → parked in the bounded backlog, to be promoted when
    ///   a slot frees.
    pub async fn add_job(&self, batch_number: u64, job_data: ZiskJobData) {
        let mut state = self.state.lock().await;
        if state.jobs.contains_key(&batch_number) {
            tracing::debug!(batch = batch_number, "ZiSK job already known, skipping add");
            return;
        }
        if state.active() >= MAX_TOTAL_JOBS {
            Self::park_in_backlog(&mut state, batch_number, job_data, self.multi_proof_mode);
            Self::record_queue_gauges(&state);
            return;
        }

        tracing::info!(
            batch = batch_number,
            zisk_data_bytes = job_data.zisk_data.len(),
            "ZiSK job added"
        );
        state.jobs.insert(
            batch_number,
            BatchJob::Pending {
                data: job_data,
                failures: 0,
            },
        );
        Self::record_queue_gauges(&state);
    }

    /// The sealed input bytes, while this lane still holds the batch. `None`
    /// once its proof was accepted or the batch was discarded. Backs the debug
    /// `/ZiSK/{batch}/peek` endpoint.
    pub async fn peek_input(&self, batch_number: u64) -> Option<Vec<u8>> {
        let state = self.state.lock().await;
        state
            .jobs
            .get(&batch_number)
            .map(|job| job.data().zisk_data.clone())
    }

    /// Debug view of one held job with the same ZiSK identity exposed by
    /// `pick`, so the peek endpoint cannot accidentally report Airbender's VK.
    pub async fn peek_job(&self, batch_number: u64) -> Option<ZiskJob> {
        let state = self.state.lock().await;
        let data = state.jobs.get(&batch_number)?.data();
        Some(ZiskJob {
            batch_number,
            vk_hash: Self::vk_hash(data),
            zisk_data: data.zisk_data.clone(),
        })
    }

    fn vk_hash(data: &ZiskJobData) -> String {
        let protocol_version = data.batch_metadata.batch_info.protocol_version.clone();
        ZiskProvingVersion::try_from(protocol_version.clone())
            .map(|version| version.verification_key_hash().to_string())
            .unwrap_or_else(|_| {
                tracing::warn!(%protocol_version, "ZiSK proving version missing");
                String::new()
            })
    }

    /// Pick the next available ZiSK job for a prover.
    pub async fn pick_next_job(&self, prover_id: &str) -> Option<ZiskJob> {
        self.pick_next_job_with_capabilities(prover_id, None).await
    }

    /// Pick the next pending job whose complete ZiSK proving identity is in
    /// the prover's declaration. `None` preserves compatibility with older
    /// daemons, which did not send capabilities.
    pub async fn pick_next_job_with_capabilities(
        &self,
        prover_id: &str,
        supported_vk_hashes: Option<&[B256]>,
    ) -> Option<ZiskJob> {
        let now = std::time::Instant::now();
        let mut state = self.state.lock().await;

        // Return timed-out leases to the pending queue.
        let timed_out: Vec<u64> = state
            .jobs
            .iter()
            .filter(|(_, job)| match job {
                BatchJob::Assigned { lease, .. } => {
                    now.duration_since(lease.since) >= self.assignment_timeout
                }
                _ => false,
            })
            .map(|(&batch, _)| batch)
            .collect();
        for batch in timed_out {
            let Some(BatchJob::Assigned {
                data,
                lease,
                failures,
            }) = state.jobs.remove(&batch)
            else {
                unreachable!("filtered to assigned jobs");
            };
            tracing::warn!(
                batch,
                old_prover = lease.prover_id,
                "ZiSK job timed out, returning to pending"
            );
            state
                .jobs
                .insert(batch, BatchJob::Pending { data, failures });
        }

        // Offer the lowest pending batch: batch order is proving order.
        let batch_number = state
            .batches_in(|job| {
                let BatchJob::Pending { data, .. } = job else {
                    return false;
                };
                let Ok(version) = ZiskProvingVersion::try_from(
                    data.batch_metadata.batch_info.protocol_version.clone(),
                ) else {
                    return false;
                };
                let Some(supported_vk_hashes) = supported_vk_hashes else {
                    return true;
                };
                supported_vk_hashes.contains(&version.verification_key_hash())
            })
            .next()?;
        let Some(BatchJob::Pending {
            data: job_data,
            failures,
        }) = state.jobs.remove(&batch_number)
        else {
            unreachable!("filtered to pending jobs");
        };

        let vk_hash = Self::vk_hash(&job_data);

        let zisk_data = job_data.zisk_data.clone();
        let generation = state.issue_generation();
        state.jobs.insert(
            batch_number,
            BatchJob::Assigned {
                data: job_data,
                lease: Lease {
                    prover_id: prover_id.to_string(),
                    since: now,
                    generation,
                },
                failures,
            },
        );
        Self::record_queue_gauges(&state);

        tracing::info!(batch = batch_number, prover_id, "ZiSK job assigned");

        Some(ZiskJob {
            batch_number,
            vk_hash,
            zisk_data,
        })
    }

    /// Return the assignment captured by this submission, or reject a result
    /// that arrived after timeout/reassignment. The latter must not consume the
    /// newer lease or publish a second completion signal.
    fn assigned_for_generation(
        state: &ZiskJobState,
        batch_number: u64,
        generation: u64,
    ) -> Result<(&ZiskJobData, u32), ZiskSubmitError> {
        match state.jobs.get(&batch_number) {
            Some(BatchJob::Assigned {
                data,
                lease,
                failures,
            }) if lease.generation == generation => Ok((data, *failures)),
            _ => {
                ZISK_LANE_METRICS.superseded_submissions.inc();
                Err(ZiskSubmitError::Superseded(batch_number))
            }
        }
    }

    /// Consume exactly the assignment captured by this submission. Call only
    /// after every cancellable prerequisite has completed; removal and the
    /// following lifecycle transition happen under the same state guard.
    fn take_assigned_generation(
        state: &mut ZiskJobState,
        batch_number: u64,
        generation: u64,
    ) -> Result<(ZiskJobData, u32), ZiskSubmitError> {
        Self::assigned_for_generation(state, batch_number, generation)?;
        let Some(BatchJob::Assigned { data, failures, .. }) = state.jobs.remove(&batch_number)
        else {
            unreachable!("the generation check required an assigned job");
        };
        Ok((data, failures))
    }

    /// Phase 6: offer the accepted stream to the aggregation lane. This is
    /// idempotent per batch, so cancellation can leave the job assigned and a
    /// retry can safely repeat the handoff.
    async fn hand_to_aggregation(
        &self,
        batch_number: u64,
        proof: &[u8],
        parsed: &crate::vadcop_stream::VadcopStreamPublics,
        protocol_version: ProtocolSemanticVersion,
    ) -> AggregationInputOutcome {
        self.aggregation_sink
            .on_proof_completed(
                batch_number,
                AggregationInput {
                    stream: proof.to_vec(),
                    protocol_version,
                    program_vk: parsed.program_vk,
                    vadcop_vk: parsed.vadcop_vk,
                    commitment: parsed.commitment,
                },
            )
            .await
    }

    /// Phase 5: one proof system disagrees with the other about this batch.
    ///
    /// Which one is a separate question, answered by classifying the submitted
    /// commitment against the expected one and this node's own shadow
    /// execution. Always counted and logged; halting is reserved for a
    /// corroborated divergence, and the retry policy depends on the mode.
    /// The job stays assigned until this function acquires the state lock. It
    /// then performs the entire generation-checked transition without an await.
    async fn on_commitment_mismatch(&self, evidence: MismatchEvidence<'_>) -> ZiskSubmitError {
        let MismatchEvidence {
            batch_number,
            generation,
            prover_id,
            commitment,
            expected_commitment,
            proof_is_verified,
        } = evidence;
        let mut state = self.state.lock().await;
        let (job_data, failures) =
            match Self::take_assigned_generation(&mut state, batch_number, generation) {
                Ok(assignment) => assignment,
                Err(error) => return error,
            };
        // One proof system is wrong; which one is a separate question.
        // Always count + log, then classify against the local arbiter.
        let msg = format!("commitment mismatch: ZiSK={commitment}, expected={expected_commitment}");
        ZISK_LANE_METRICS.commitment_mismatches.inc();
        let classification = classify_mismatch(
            job_data.seal_shadow_commitment,
            commitment,
            expected_commitment,
        );
        tracing::error!(batch = batch_number, ?classification, "{msg}");
        if proof_is_verified && classification == MismatchClassification::WrongResult {
            // The submission is valid but its result disagrees with what
            // this node's own guest execution produced: the prover, not the
            // proof system, is at fault. Requeue below and let another
            // prover answer.
            ZISK_LANE_METRICS.wrong_result_submissions.inc();
            tracing::error!(
                batch = batch_number,
                prover_id,
                "verified ZiSK proof carries a wrong result — local re-execution agrees \
                 with the expected commitment; treating this as prover misbehavior"
            );
        }
        // Halting is reserved for a corroborated divergence: a verified
        // proof whose result an independent local re-execution reproduces.
        // Anything less — an unverified submission, or one no local run
        // backs — is a caller-supplied byte string, and must never be able
        // to stop the node.
        if should_halt(proof_is_verified, classification)
            && let Some(halt) = self
                .halt_on_mismatch
                .lock()
                .expect("halt sender lock")
                .take()
        {
            let _ = halt.send(format!(
                "ZiSK corroborated divergence on batch {batch_number} (verified proof \
                 reproduced by local execution): {msg}"
            ));
            Self::record_queue_gauges(&state);
        } else {
            // Retry a faulty or transient prover; what a deterministic
            // divergence costs depends on the mode.
            let required = self.multi_proof_mode == MultiProofMode::Required;
            let attempts = failures + 1;
            if attempts >= MAX_COMMITMENT_MISMATCH_ATTEMPTS && required {
                // Only a proof clears this batch's gate entry, so dropping
                // the job would stop commits for good with nothing left to
                // retry. Keep it and re-alarm at every threshold instead: a
                // replaced prover or guest build converges.
                state.jobs.insert(
                    batch_number,
                    BatchJob::Pending {
                        data: job_data,
                        failures: 0,
                    },
                );
                Self::record_queue_gauges(&state);
                ZISK_LANE_METRICS.unprovable.inc();
                tracing::error!(
                    batch = batch_number,
                    attempts = MAX_COMMITMENT_MISMATCH_ATTEMPTS,
                    "ZiSK lane unprovable: batch commitment mismatched on every attempt. \
                     The second proof system gates settlement, so the batch stays at the \
                     commit gate and the job stays queued for retry — commits stall until \
                     one proof system is fixed. Investigate."
                );
            } else if attempts >= MAX_COMMITMENT_MISMATCH_ATTEMPTS {
                // job_data intentionally dropped: not reinserted anywhere,
                // so the slot is freed — let a parked input take it.
                Self::promote_from_backlog(&mut state, self.multi_proof_mode);
                Self::record_queue_gauges(&state);
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
                state.jobs.insert(
                    batch_number,
                    BatchJob::Pending {
                        data: job_data,
                        failures: attempts,
                    },
                );
                Self::record_queue_gauges(&state);
            }
        }
        ZiskSubmitError::CommitmentMismatch
    }

    /// Phase 1 of a submission: the wire shape. Aggregated mode expects the
    /// raw `vadcop_final` stream, which carries its own publics, so the
    /// separate public-values field must be empty.
    fn parse_submission(
        proof: &[u8],
        public_values: &[u8],
    ) -> Result<crate::vadcop_stream::VadcopStreamPublics, ZiskSubmitError> {
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
        parse_vadcop_final_stream(proof).map_err(ZiskSubmitError::MalformedProof)
    }

    /// Phase 2: the keys the batch must have been proven under, read from the
    /// assigned job WITHOUT consuming it — a drift must leave the job assigned
    /// so it times out back to pending for another prover.
    async fn expected_keys_for(
        &self,
        batch_number: u64,
    ) -> Result<(Option<ZiskVkSet>, ProtocolSemanticVersion, u64), ZiskSubmitError> {
        let state = self.state.lock().await;
        let Some(BatchJob::Assigned { data, lease, .. }) = state.jobs.get(&batch_number) else {
            return Err(ZiskSubmitError::UnknownJob(batch_number));
        };
        let version = data.batch_metadata.batch_info.protocol_version.clone();
        Ok((
            self.expected_vks.get(&version).copied(),
            version,
            lease.generation,
        ))
    }

    /// Phase 3: the cryptographic check of the STARK layer. Seconds of pure
    /// CPU, so it runs on the blocking pool like the Airbender lane's, bounded
    /// by `verification_slots`, and outside every state lock. The stream rides
    /// in and out of the task, so verifying costs no copy of it. A verifier
    /// panic on malformed bytes is contained for the same reason a rejection
    /// is: an unverified submission is bytes a caller sent, and must neither
    /// take the node down nor consume a job.
    async fn verify_stream(
        &self,
        proof: Vec<u8>,
    ) -> Result<(Vec<u8>, Result<(), String>), ZiskSubmitError> {
        let _slot = self
            .verification_slots
            .acquire()
            .await
            .expect("verification semaphore is never closed");
        let (returned, outcome) = tokio::task::spawn_blocking(move || {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                zisk_verifier::verify_vadcop_final_stream(&proof)
            }));
            (proof, outcome)
        })
        .await
        .map_err(|e| ZiskSubmitError::ProofVerificationFailed(e.to_string()))?;
        let verdict = match outcome {
            Ok(verdict) => verdict.map_err(|e| e.to_string()),
            Err(_) => Err("ZiSK verifier panicked".to_string()),
        };
        Ok((returned, verdict))
    }

    /// Submit a per-batch ZiSK proof: the raw `vadcop_final` stream, with
    /// empty `public_values` (the stream carries its publics).
    ///
    /// Validates the shape, the program-VK tripwire, and the batch commitment
    /// against the metadata captured at job creation, then buffers the stream
    /// in the aggregation manager as range input and lets the job go.
    pub async fn submit_proof(
        &self,
        batch_number: u64,
        mut proof: Vec<u8>,
        public_values: Vec<u8>,
        prover_id: &str,
    ) -> Result<(), ZiskSubmitError> {
        // 1. Wire shape: nothing below may run on bytes that do not parse.
        let parsed = Self::parse_submission(&proof, &public_values)?;
        let (reported_vk, reported_vadcop_vk, commitment) =
            (parsed.program_vk, parsed.vadcop_vk, parsed.commitment);

        // 2. Which keys the batch must have been proven under. An unknown batch
        //    is rejected here, before anything below spends CPU on it: the
        //    endpoint is unauthenticated.
        let (expected_vks, job_protocol_version, generation) =
            self.expected_keys_for(batch_number).await?;

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
            // Protocol upgrades can arrive after startup, so the compiled
            // release registry is checked again for every submission. Shadow
            // mode must also fail closed: accepting an unknown guest there
            // would make its comparison signal meaningless.
            let version = job_protocol_version.clone();
            ZISK_LANE_METRICS.vk_drift.inc();
            tracing::error!(
                batch = batch_number,
                prover_id,
                %reported_vk,
                %version,
                "this server binary has no compiled ZiSK manifest for the batch's protocol \
                 version — rejecting the submission"
            );
            return Err(ZiskSubmitError::MissingVersionKeys { version });
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

        // 3. Verify before consuming the job and before judging the
        //    commitment: an unverified submission is bytes a caller sent, so it
        //    must reach neither the job nor the halt path below.
        let proof_is_verified = if self.proof_verification_enabled {
            let (returned, verdict) = self.verify_stream(proof).await?;
            proof = returned;
            if let Err(e) = verdict {
                ZISK_LANE_METRICS.proof_verification_failures.inc();
                tracing::error!(
                    batch = batch_number,
                    prover_id,
                    "ZiSK proof rejected by off-chain verification: {e}"
                );
                return Err(ZiskSubmitError::ProofVerificationFailed(e));
            }
            true
        } else {
            false
        };

        // Validate the batch commitment against the metadata captured at
        // seal, using the guest lib's own hash functions. The assignment is
        // only borrowed here: cancellation or supersession still leaves a
        // complete job for the timeout/retry path.
        let (expected_commitment, job_age) = {
            let state = self.state.lock().await;
            let (job_data, _) = Self::assigned_for_generation(&state, batch_number, generation)?;
            let proving_version = job_data
                .batch_metadata
                .proving_version()
                .map_err(|e| ZiskSubmitError::PublicInputUnderivable(format!("{e:#}")))?;
            let prev = &job_data.batch_metadata.previous_stored_batch_info;
            (
                crate::commitment::expected_zisk_public_input(
                    proving_version,
                    &prev.state_commitment,
                    &job_data.batch_metadata.batch_info,
                    self.chain_id,
                )
                .map_err(|e| ZiskSubmitError::PublicInputUnderivable(format!("{e:#}")))?,
                job_data.added_at.elapsed(),
            )
        };
        if commitment != expected_commitment {
            return Err(self
                .on_commitment_mismatch(MismatchEvidence {
                    batch_number,
                    generation,
                    prover_id,
                    commitment,
                    expected_commitment,
                    proof_is_verified,
                })
                .await);
        }

        tracing::info!(
            batch = batch_number,
            prover_id,
            zisk_proof_bytes = proof.len(),
            aggregated = true,
            "ZiSK proof accepted"
        );

        // Aggregated mode: buffer a copy as range input for the aggregation
        // manager. When the input is not buffered do NOT park a completion
        // marker — a marker for an absent input would strand the range. The two
        // not-buffered cases differ, so they are handled apart (see
        // `AggregationInputOutcome`): a `BufferFull` input is re-parked so it is
        // retried once buffer space frees, while a `BelowFloor` input (its range
        // already went downstream) is dropped.
        // 6. The aggregation handoff is idempotent. Keep the assignment while
        // awaiting it, so cancellation leaves a complete job that can retry.
        let handoff = self
            .hand_to_aggregation(batch_number, &proof, &parsed, job_protocol_version)
            .await;

        // The commit gate gets exactly one identity-bearing notice. Reserve
        // capacity before consuming the assignment; publishing through the
        // permit after the state commit is synchronous and cannot be cancelled.
        let ready_permit = if handoff == AggregationInputOutcome::Buffered {
            match &self.batch_ready {
                Some(sender) => Some(
                    sender
                        .reserve()
                        .await
                        .map_err(|_| ZiskSubmitError::CompletionUnavailable(batch_number))?,
                ),
                None => None,
            }
        } else {
            None
        };

        let mut state = self.state.lock().await;
        let (job_data, _) = Self::take_assigned_generation(&mut state, batch_number, generation)?;
        match handoff {
            AggregationInputOutcome::Buffered => {
                // The aggregation lane owns the stream now, so this lane is
                // done with the batch and its active slot returns immediately.
                state.proofs_accepted += 1;
                Self::promote_from_backlog(&mut state, self.multi_proof_mode);
                Self::record_queue_gauges(&state);
                drop(state);
                if let Some(permit) = ready_permit {
                    permit.send(batch_number);
                }
                ZISK_LANE_METRICS.time_to_submit.observe(job_age);
            }
            AggregationInputOutcome::BufferFull => {
                // The input was not accepted, so retain the original sealed
                // data for a later retry once aggregation capacity drains.
                Self::park_in_backlog(&mut state, batch_number, job_data, self.multi_proof_mode);
                Self::record_queue_gauges(&state);
                tracing::warn!(
                    batch = batch_number,
                    "ZiSK aggregation buffer full — re-parked the input for retry"
                );
            }
            AggregationInputOutcome::BelowFloor => {
                // The range already went downstream. The sink counted the lost
                // coverage; this job can no longer contribute and frees its slot.
                Self::promote_from_backlog(&mut state, self.multi_proof_mode);
                Self::record_queue_gauges(&state);
            }
        }
        Ok(())
    }

    /// The batches at or below `batch_to` went downstream, so nothing parked
    /// below the cut can still join a range.
    ///
    /// Only the backlog is swept, and only where a settled batch can never
    /// compose: an accepted proof already freed its slot. Shadow proving keeps
    /// its parked inputs — settlement never waited for them, so they still
    /// prove and are still verified, late.
    pub async fn on_batches_settled(&self, batch_to: u64) {
        {
            let sink = &self.aggregation_sink;
            sink.on_batches_settled(batch_to).await;
        }
        if self.multi_proof_mode == MultiProofMode::Shadow {
            return;
        }
        let mut state = self.state.lock().await;
        let stale_backlog: Vec<u64> = state
            .batches_in(|job| matches!(job, BatchJob::Backlogged(_)))
            .filter(|&b| b <= batch_to)
            .collect();
        for batch in &stale_backlog {
            state.jobs.remove(batch);
        }
        if !stale_backlog.is_empty() {
            tracing::info!(
                batch_to,
                discarded_backlog = stale_backlog.len(),
                "swept parked ZiSK inputs for batches already sent downstream"
            );
        }
        // Promote unconditionally: the sink released aggregation-buffer
        // capacity just above, and a job re-parked on `BufferFull` gave its
        // active slot back when it parked. That job sits above the settled cut,
        // so sweeping finds nothing — and without a promotion here nothing else
        // would wake it, since the commit gate stops admitting new batches
        // exactly while it waits.
        Self::promote_from_backlog(&mut state, self.multi_proof_mode);
        Self::record_queue_gauges(&state);
    }

    /// Drop all ZiSK state for a batch range. Used by the fake-SNARK pass so
    /// batches consumed without a real Airbender SNARK (fake-prover
    /// environments, pre-V6 replay) don't leave orphaned jobs behind.
    pub async fn discard_batches(&self, batch_from: u64, batch_to: u64) {
        // The fake-SNARK pass consumes the lowest in-flight batches, so the
        // aggregation lane treats this as an up-to cut as well.
        {
            let sink = &self.aggregation_sink;
            sink.discard_up_to(batch_to).await;
        }
        let mut state = self.state.lock().await;
        let mut discarded = 0usize;
        for batch in batch_from..=batch_to {
            discarded += usize::from(state.jobs.remove(&batch).is_some());
        }
        if discarded > 0 {
            // Freed active slots let parked inputs outside the range go active.
            Self::promote_from_backlog(&mut state, self.multi_proof_mode);
            tracing::debug!(batch_from, batch_to, discarded, "discarded ZiSK lane state");
            Self::record_queue_gauges(&state);
        }
    }

    /// Check if there are pending or assigned ZiSK jobs.
    pub async fn has_pending_jobs(&self) -> bool {
        let state = self.state.lock().await;
        state.active() > 0
    }
}

#[cfg(test)]
mod tests;
