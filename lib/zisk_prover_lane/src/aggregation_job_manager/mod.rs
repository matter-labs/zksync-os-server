//! Job manager for the ZiSK aggregation stage.
//!
//! The per-batch lane produces `vadcop_final` streams; this manager buffers
//! them and collapses a RANGE of them into one aggregation job, which the
//! aggregator guest (`zksync-os-zisk/guest-aggregator`) verifies in-zkVM,
//! committing the L1 binding digest over their chained batch public inputs.
//!
//! # Range identity
//!
//! Aggregation ranges are exactly the ranges the Airbender SNARK jobs cover,
//! never independently counted: the SNARK pick and submission both call
//! [`Self::note_snark_range`], so the rendezvous pairs one Airbender range
//! SNARK with one ZiSK range proof of the same bounds. A timed-out SNARK range
//! may be re-picked with different bounds, so several overlapping ranges can be
//! tracked at once; whichever the Airbender submission settles on wins, and
//! [`Self::take_completed`] retires everything it overlaps. Buffered per-batch
//! inputs are shared by overlapping ranges until a range consumes them or a
//! discard passes them.
//!
//! Under [`MultiProofMode::Shadow`] nothing composes: an accepted proof retires
//! its range on the spot, and settlement only records how late the verification
//! was, which keeps a lagging lane covering the batches it settles behind.

use alloy::primitives::{B256, keccak256};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

use crate::commitment::ZISK_PUBLIC_VALUES_BYTES;
use crate::job_manager::{MultiProofMode, ZiskVkSet};
use crate::metrics::ZISK_LANE_METRICS;
use crate::range::BatchRange;
use zksync_os_batch_types::batcher_model::ZISK_SNARK_PROOF_BYTES;
use zksync_os_types::ProtocolSemanticVersion;

/// Cap on buffered per-batch inputs (~330 KiB each). When full, new
/// arrivals are dropped with a warning; the SNARK-side wait timeout is the
/// operator backstop if aggregation provers stay offline.
pub(crate) const MAX_BUFFERED_INPUTS: usize = 64;

/// Completed aggregated proofs parked for the rendezvous. Ranges are
/// consumed in order by the Airbender lane, so more than a couple parked
/// ranges means the Airbender lane is stuck — cap and complain.
const MAX_COMPLETED: usize = 16;

/// Cap on ranges tracked while their inputs are still incomplete. In shadow
/// proving the floor advances only when a range is verified, so a lane whose
/// provers stay offline would otherwise track one range per settled Airbender
/// SNARK forever, and hold their inputs with them. The lowest tracked range is
/// retired when the cap is passed, which counts its buffered inputs as lost
/// coverage. Unreachable while the multi-proof is required — settlement waits
/// for the range there.
const MAX_TRACKED_RANGES: usize = 128;

/// Give-up threshold for a range whose submitted proof fails binding-digest
/// validation. A DETERMINISTIC mismatch (a real divergence, or inputs the
/// aggregator cannot reconcile) would otherwise re-prove the whole range
/// forever. After this many misses the range is abandoned instead of
/// requeued. Mirrors the per-batch lane's `MAX_COMMITMENT_MISMATCH_ATTEMPTS`;
/// > 1 so a genuinely flaky prover still gets retries.
const MAX_DIGEST_MISMATCH_ATTEMPTS: u32 = 3;

/// Aggregated verification is small compared with per-batch STARK
/// verification, but it still runs on Tokio's shared blocking pool.
const MAX_CONCURRENT_VERIFICATIONS: usize = 4;

/// Run an untrusted native verifier without blocking a Tokio worker. The
/// owned permit lives inside the blocking task, so cancelling the caller does
/// not release capacity while native code is still running.
async fn run_blocking_verifier<T, E, F>(
    permit: tokio::sync::OwnedSemaphorePermit,
    input: T,
    verify: F,
) -> Result<(T, Result<(), String>), String>
where
    T: Send + 'static,
    E: ToString + Send + 'static,
    F: FnOnce(&T) -> Result<(), E> + Send + 'static,
{
    let (input, outcome) = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| verify(&input)));
        (input, outcome)
    })
    .await
    .map_err(|error| error.to_string())?;
    let verdict = match outcome {
        Ok(verdict) => verdict.map_err(|error| error.to_string()),
        Err(_) => Err("ZiSK verifier panicked".to_string()),
    };
    Ok((input, verdict))
}

/// One buffered per-batch aggregation input: the validated `vadcop_final`
/// stream plus the values extracted/validated at per-batch submission.
#[derive(Clone)]
pub struct AggregationInput {
    /// The full serialized proof stream the aggregator guest verifies.
    pub stream: Vec<u8>,
    /// Protocol version of the batch this input proves. Keys the inner vadcop
    /// VK tripwire, so each input is checked against the guest build of its own
    /// version.
    pub protocol_version: ProtocolSemanticVersion,
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
#[derive(Clone)]
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
    #[error("submission for aggregation range {from}..{to} was superseded by a newer lease")]
    Superseded { from: u64, to: u64 },
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
    #[error(
        "no ZiSK verification keys configured for protocol version {version} in this range; \
         the second proof system gates settlement, so an unpinned guest build is not accepted"
    )]
    MissingVersionKeys { version: ProtocolSemanticVersion },
}

/// What the buffered inputs of a range say about it, before any state is
/// touched.
enum RangeCheck {
    /// The inputs agree and bind to this digest.
    Ok { digest: B256 },
    /// An input's protocol version has no pinned key set, which `Required`
    /// refuses: this range's proof goes on L1.
    UnpinnedVersion(ProtocolSemanticVersion),
    /// An input was proven under a different recursive setup than the
    /// configured `rootCVadcopFinal`.
    InnerVadcopDrift { reported: B256, expected: B256 },
    /// The inputs cannot form a binding digest at all — they disagree on the
    /// keys they were proven under.
    NoDigest(String),
}

/// Where one range is in the aggregation lane.
///
/// One value per range, so a range cannot be in two places at once and a
/// failure count cannot outlive the range it counts for.
enum RangeJob {
    /// Derived from a SNARK range, but not all of its per-batch inputs have
    /// arrived yet.
    AwaitingInputs,
    /// Every input is buffered: the range can be offered to an aggregator.
    Pending { failures: u32 },
    /// Leased to an aggregator until it submits or times out.
    Assigned { lease: Lease, failures: u32 },
    /// A submitted proof is being verified. Its timed generation both prevents
    /// concurrent re-registration and lets a cancelled request be recovered
    /// without accepting a late result over a newer attempt.
    Verifying {
        generation: u64,
        since: Instant,
        failures: u32,
    },
    /// Verified aggregated proof, waiting for its Airbender SNARK to compose.
    Completed(CompletedAggregatedProof),
}

/// Who holds a range and since when.
struct Lease {
    prover_id: String,
    since: Instant,
    generation: u64,
}

struct State {
    /// Buffered per-batch inputs, keyed by batch number. Kept in their own map
    /// because ranges can overlap and re-keyed ranges reuse them. Retained
    /// until a completed range is taken (rendezvous) or a discard passes them.
    inputs: BTreeMap<u64, AggregationInput>,
    /// One entry per range this lane knows about, in range order — which is
    /// the order ranges settle in.
    ranges: BTreeMap<BatchRange, RangeJob>,
    /// Batches at or below this can never join a range: they were either
    /// consumed by a taken range or sent downstream without aggregation.
    floor: u64,
    /// Highest batch that went downstream on L1. Shadow proving only, where
    /// settlement does not retire the range: it marks a range verified after
    /// its batches settled, which is the coverage the lane would otherwise
    /// lose silently. The floor stays put, so those batches keep proving.
    settled_up_to: u64,
    /// Monotonic across every range assignment in this manager process.
    next_generation: u64,
}

/// Why the lane is retiring everything at or below a cut.
#[derive(Debug, Clone, Copy)]
enum RetireCause {
    /// An aggregated proof over this range was verified, so those batches have
    /// their second-proof coverage. Anything else the cut drops does not.
    Verified { range: BatchRange },
    /// The batches went downstream with no aggregated ZiSK proof at all, so
    /// everything the cut drops is lost coverage.
    Unverified,
}

impl RetireCause {
    fn covers(&self, batch: u64) -> bool {
        matches!(self, Self::Verified { range } if range.contains(batch))
    }

    fn reason(&self) -> &'static str {
        match self {
            Self::Verified { .. } => "batches covered by a verified aggregated ZiSK proof",
            Self::Unverified => "batches sent downstream without aggregation",
        }
    }
}

impl State {
    fn issue_generation(&mut self) -> u64 {
        let generation = self.next_generation;
        self.next_generation = self
            .next_generation
            .checked_add(1)
            .expect("ZiSK aggregation generation overflow");
        generation
    }

    fn recover_expired_verifications(&mut self, now: Instant, timeout: Duration) {
        let expired: Vec<BatchRange> = self
            .ranges
            .iter()
            .filter_map(|(&range, job)| match job {
                RangeJob::Verifying { since, .. } if now.duration_since(*since) >= timeout => {
                    Some(range)
                }
                _ => None,
            })
            .collect();
        for range in expired {
            let Some(RangeJob::Verifying { failures, .. }) = self.ranges.remove(&range) else {
                unreachable!("filtered to verifying ranges");
            };
            ZISK_LANE_METRICS.aggregation_verification_timeouts.inc();
            tracing::warn!(%range, "ZiSK aggregation verification timed out, returning to queue");
            self.ranges.insert(range, RangeJob::Pending { failures });
        }
    }

    fn knows_range(&self, range: BatchRange) -> bool {
        self.ranges.contains_key(&range)
    }

    /// Ranges in a given lifecycle state, in range order.
    fn ranges_in<'a>(
        &'a self,
        is: impl Fn(&RangeJob) -> bool + 'a,
    ) -> impl Iterator<Item = BatchRange> + 'a {
        self.ranges
            .iter()
            .filter(move |(_, job)| is(job))
            .map(|(&range, _)| range)
    }

    fn count(&self, is: impl Fn(&RangeJob) -> bool) -> usize {
        self.ranges.values().filter(|job| is(job)).count()
    }

    /// Requeue a range after a failed validation. Returns true when the range
    /// has now failed `MAX_DIGEST_MISMATCH_ATTEMPTS` times, which the caller
    /// alarms on.
    ///
    /// What that threshold means depends on the mode. In `Shadow` the range is
    /// dropped: coverage is sheddable and a deterministic failure would
    /// otherwise re-prove it forever. In `Required` the range is the only thing
    /// that can release the Airbender proof parked against it, and the batches
    /// behind it are held at the commit gate — dropping it would stall
    /// settlement with nothing left to retry. So it is kept pickable and the
    /// counter restarts, re-alarming at every threshold crossing until a
    /// repaired prover lands it.
    fn requeue_or_abandon(
        &mut self,
        range: BatchRange,
        failures: u32,
        mode: MultiProofMode,
    ) -> bool {
        let attempts = failures + 1;
        if attempts < MAX_DIGEST_MISMATCH_ATTEMPTS {
            self.ranges
                .insert(range, RangeJob::Pending { failures: attempts });
            return false;
        }
        if mode == MultiProofMode::Required {
            // The counter restarts so the alarm fires again at the next
            // threshold rather than on every submission.
            self.ranges.insert(range, RangeJob::Pending { failures: 0 });
        } else {
            self.ranges.remove(&range);
        }
        true
    }

    /// Make every range whose inputs have all arrived offerable, in range
    /// order.
    fn form_ready_ranges(&mut self) {
        let ready: Vec<BatchRange> = self
            .ranges
            .iter()
            .filter(|(range, job)| {
                matches!(job, RangeJob::AwaitingInputs)
                    && range.batches().all(|b| self.inputs.contains_key(&b))
            })
            .map(|(&range, _)| range)
            .collect();
        for range in ready {
            self.ranges.insert(range, RangeJob::Pending { failures: 0 });
            tracing::info!(%range, "ZiSK aggregation range formed");
        }
    }

    /// Drop all tracking for ranges starting at or below `batch_to` and
    /// all inputs at or below it, and advance the floor. Ranges straddling
    /// the cut are dropped whole (they can never rendezvous), but their
    /// above-the-cut inputs stay: re-keyed ranges may reuse them.
    ///
    /// Every dropped input the `cause` does not cover is a batch that keeps no
    /// second-proof verification, so it raises the coverage-lost alarm.
    fn retire_up_to(&mut self, batch_to: u64, cause: RetireCause) {
        let reason = cause.reason();
        let stale: Vec<BatchRange> = self
            .ranges
            .keys()
            .copied()
            .filter(|range| range.from() <= batch_to)
            .collect();
        for range in stale {
            let dropped = self.ranges.remove(&range);
            let what = match dropped {
                Some(RangeJob::Completed(_)) => "parked aggregated ZiSK proof dropped",
                _ => "ZiSK aggregation range dropped",
            };
            tracing::info!(%range, reason, "{what}");
        }
        let stale_inputs: Vec<u64> = self.inputs.range(..=batch_to).map(|(&b, _)| b).collect();
        let mut lost = 0u64;
        for b in stale_inputs {
            self.inputs.remove(&b);
            lost += u64::from(!cause.covers(b));
        }
        if lost > 0 {
            ZISK_LANE_METRICS.coverage_lost.inc_by(lost);
            tracing::error!(
                batch_to,
                lost,
                reason,
                "ZiSK coverage lost: buffered per-batch inputs retired without a range proof \
                 over them — those batches keep no second-proof verification"
            );
        }
        self.floor = self.floor.max(batch_to);
    }

    fn record_gauges(&self) {
        ZISK_LANE_METRICS
            .aggregation_inputs_buffered
            .set(self.inputs.len() as u64);
        ZISK_LANE_METRICS
            .aggregated_proofs_awaiting_snark
            .set(self.count(|job| matches!(job, RangeJob::Completed(_))) as u64);
    }
}

/// Manages ZiSK aggregation jobs with the same pick/submit assignment
/// model as the other prover stages.
/// What the aggregation lane checks and how long it waits.
pub struct ZiskAggregationLaneConfig {
    /// Batches per range. Equals the Airbender `max_fris_per_snark`, since a
    /// range covers exactly one SNARK job.
    pub range_size: usize,
    /// If a prover does not submit within this, the range is re-offered.
    pub assignment_timeout: Duration,
    /// If server-side verification does not finish within this, its range is
    /// re-offered independently of the external prover lease timeout.
    pub verification_timeout: Duration,
    /// Expected AGGREGATOR guest program VK.
    pub expected_program_vk: Option<B256>,
    /// Expected INNER per-batch VK sets per protocol version.
    pub expected_inner_vks: HashMap<ProtocolSemanticVersion, ZiskVkSet>,
    pub proof_verification_enabled: bool,
    /// What losing a range's proof costs, together with what that answer
    /// requires — the same pairing as the per-batch lane's.
    pub mode: ZiskAggregationMode,
}

/// The aggregation lane's mode and everything that mode needs.
pub enum ZiskAggregationMode {
    /// Nothing composes: the verification is the end of the range, so no
    /// parked proof waits for an Airbender half and nothing announces one.
    Shadow,
    /// A range settles as a composed multi-proof, so a parked proof has to
    /// announce itself to the stage that composes.
    Required {
        range_ready: tokio::sync::mpsc::Sender<BatchRange>,
    },
}

impl ZiskAggregationMode {
    fn multi_proof_mode(&self) -> MultiProofMode {
        match self {
            Self::Shadow => MultiProofMode::Shadow,
            Self::Required { .. } => MultiProofMode::Required,
        }
    }

    fn into_range_ready(self) -> Option<tokio::sync::mpsc::Sender<BatchRange>> {
        match self {
            Self::Shadow => None,
            Self::Required { range_ready } => Some(range_ready),
        }
    }
}

pub struct ZiskAggregationJobManager {
    state: Mutex<State>,
    /// Upper bound on range width; set to `prover_api.max_fris_per_snark`,
    /// so wider SNARK ranges cannot exist.
    range_size: u64,
    assignment_timeout: Duration,
    verification_timeout: Duration,
    /// Expected AGGREGATOR guest program VK (`public_values[0..32]` of an
    /// aggregated proof). When set, a submission with a different VK is
    /// rejected and counted — the prover runs a different aggregator
    /// build. Unset: the reported VK is only logged.
    expected_program_vk: Option<B256>,
    /// Expected INNER per-batch VK sets per protocol version — the same map the
    /// per-batch lane checks against. Every buffered input of a range is
    /// checked against the vadcop-final VK (`rootCVadcopFinal`) of its own
    /// protocol version, so an upgrade window where two versions coexist keeps
    /// the tripwire armed for both. A drift rejects the range and is counted
    /// (`zisk_lane_aggregated_vadcop_vk_drift`); the same VK is pinned on L1.
    /// No entry for an input's version: in `Shadow` that input's vadcop VK is
    /// not checked here, and in `Required` the range is refused rather than
    /// aggregated over an unpinned guest build.
    expected_inner_vks: HashMap<ProtocolSemanticVersion, ZiskVkSet>,
    /// Whether to run the off-chain proof verification on each aggregated-range
    /// submission. True (the production default) runs the native aggregated-
    /// range PLONK wire check. False skips only that cryptographic check; the
    /// binding-digest check above it always runs.
    proof_verification_enabled: bool,
    verification_slots: Arc<tokio::sync::Semaphore>,
    /// Announces range bounds whose aggregated proof has just been parked, so
    /// the range proving stage can compose without polling. `None` in shadow
    /// proving, where nothing composes.
    range_ready: Option<tokio::sync::mpsc::Sender<BatchRange>>,
    /// Whether a verified range proof composes into the L1 multi-proof or is
    /// the terminal event of the range. See [`Self::on_batches_settled`].
    multi_proof_mode: MultiProofMode,
}

impl ZiskAggregationJobManager {
    pub fn new(config: ZiskAggregationLaneConfig) -> Self {
        let ZiskAggregationLaneConfig {
            range_size,
            assignment_timeout,
            verification_timeout,
            expected_program_vk,
            expected_inner_vks,
            proof_verification_enabled,
            mode,
        } = config;
        let multi_proof_mode = mode.multi_proof_mode();
        let range_ready = mode.into_range_ready();
        assert!(range_size >= 1, "aggregation range_size must be >= 1");
        Self {
            state: Mutex::new(State {
                inputs: BTreeMap::new(),
                ranges: BTreeMap::new(),
                floor: 0,
                settled_up_to: 0,
                next_generation: 0,
            }),
            range_size: range_size as u64,
            assignment_timeout,
            verification_timeout,
            expected_program_vk,
            expected_inner_vks,
            proof_verification_enabled,
            verification_slots: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_VERIFICATIONS)),
            range_ready,
            multi_proof_mode,
        }
    }

    fn verification_failures_for_generation(
        state: &State,
        range: BatchRange,
        generation: u64,
    ) -> Result<u32, ZiskAggregationSubmitError> {
        match state.ranges.get(&range) {
            Some(RangeJob::Verifying {
                generation: current,
                failures,
                ..
            }) if *current == generation => Ok(*failures),
            _ => {
                ZISK_LANE_METRICS.superseded_submissions.inc();
                Err(ZiskAggregationSubmitError::Superseded {
                    from: range.from(),
                    to: range.to(),
                })
            }
        }
    }

    fn assigned_failures_for_generation(
        state: &State,
        range: BatchRange,
        generation: u64,
    ) -> Result<u32, ZiskAggregationSubmitError> {
        match state.ranges.get(&range) {
            Some(RangeJob::Assigned {
                lease, failures, ..
            }) if lease.generation == generation => Ok(*failures),
            _ => {
                ZISK_LANE_METRICS.superseded_submissions.inc();
                Err(ZiskAggregationSubmitError::Superseded {
                    from: range.from(),
                    to: range.to(),
                })
            }
        }
    }

    /// Put a range into the state a submission holds while its proof is being
    /// verified, so a test can observe that window without a slow verifier.
    #[cfg(test)]
    pub(crate) async fn mark_verifying_for_test(&self, range: BatchRange) -> u64 {
        let mut state = self.state.lock().await;
        let Some(RangeJob::Assigned {
            lease, failures, ..
        }) = state.ranges.remove(&range)
        else {
            panic!("test range must be assigned before verification");
        };
        let generation = lease.generation;
        state.ranges.insert(
            range,
            RangeJob::Verifying {
                generation,
                since: Instant::now(),
                failures,
            },
        );
        generation
    }

    /// Park a verified range proof directly, so a test can exercise the
    /// rendezvous without driving a prover through the whole submit path.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn park_completed_for_test(
        &self,
        range: BatchRange,
        proof: CompletedAggregatedProof,
    ) {
        let mut state = self.state.lock().await;
        state.ranges.insert(range, RangeJob::Completed(proof));
        state.record_gauges();
    }

    /// Register a batch range covered by an Airbender SNARK job. Called by
    /// `SnarkJobManager` at real-job pick time (so aggregation proving
    /// starts while the Airbender SNARK is still being computed) and again
    /// at SNARK submission (authoritative). Idempotent per range; ranges
    /// entirely at or below the floor are ignored.
    pub async fn note_snark_range(&self, range: BatchRange) {
        let to_batch = range.to();
        let width = range.width();
        if width > self.range_size {
            // The range width equals max_fris_per_snark, so a wider SNARK range
            // is a logic slip — track the range anyway (correctness of the
            // rendezvous over the size hint).
            tracing::warn!(
                %range,
                range_size = self.range_size,
                "SNARK range wider than the aggregation range size"
            );
        }
        let mut state = self.state.lock().await;
        state.recover_expired_verifications(Instant::now(), self.verification_timeout);
        if to_batch <= state.floor {
            return;
        }
        if state.knows_range(range) {
            return;
        }
        tracing::info!(%range, "ZiSK aggregation range registered");
        state.ranges.insert(range, RangeJob::AwaitingInputs);
        state.form_ready_ranges();
        // Retiring the lowest range also releases the inputs it was holding, so
        // a lane that never proves cannot grow this set or the input buffer
        // behind it without limit.
        // Retirement counts the retired range's inputs as lost coverage, which
        // `Required` cannot afford: those batches wait at the commit gate and
        // only this range can release them. Nothing needs retiring there —
        // the gate's admission window bounds how many ranges can be open.
        while self.multi_proof_mode != MultiProofMode::Required
            && state.count(|job| matches!(job, RangeJob::AwaitingInputs)) > MAX_TRACKED_RANGES
        {
            let Some(oldest) = state
                .ranges_in(|job| matches!(job, RangeJob::AwaitingInputs))
                .next()
            else {
                break;
            };
            tracing::error!(
                range = %oldest,
                max = MAX_TRACKED_RANGES,
                "too many ZiSK aggregation ranges waiting for their inputs — retiring the \
                 lowest (the ZiSK lane is not keeping up with the Airbender lane)"
            );
            state.retire_up_to(oldest.to(), RetireCause::Unverified);
        }
    }

    /// Feed one accepted per-batch `vadcop_final` proof (called by
    /// `ZiskJobManager::submit_proof`; stream shape, program VK, and batch
    /// commitment were already validated there). Idempotent per batch;
    /// arrivals at or below the floor or beyond the buffer cap are dropped.
    ///
    /// Reports the [`AggregationInputOutcome`]. The per-batch lane uses it to
    /// decide what to do with the batch: a `Buffered` input lets the job go;
    /// a `BufferFull` input must stay re-tryable (re-parked); a
    /// `BelowFloor` input is dropped.
    pub async fn on_proof_completed(
        &self,
        batch_number: u64,
        input: AggregationInput,
    ) -> AggregationInputOutcome {
        let mut state = self.state.lock().await;
        if batch_number <= state.floor {
            ZISK_LANE_METRICS.coverage_lost.inc();
            tracing::error!(
                batch = batch_number,
                floor = state.floor,
                "ZiSK coverage lost: a validated per-batch proof arrived below the range floor \
                 — the batch can no longer join a range, dropping the proof"
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
        state.inputs.insert(batch_number, input);
        state.form_ready_ranges();
        state.record_gauges();
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
            ranges_in_flight: state.count(|job| !matches!(job, RangeJob::Completed(_))) as u64,
            range_proofs_completed: state.count(|job| matches!(job, RangeJob::Completed(_))) as u64,
            floor: state.floor,
        }
    }

    /// Where a range currently is in the aggregation lane.
    pub async fn range_status(&self, range: BatchRange) -> ZiskAggregationRangeStatus {
        let state = self.state.lock().await;
        match state.ranges.get(&range) {
            Some(RangeJob::Completed(_)) => ZiskAggregationRangeStatus::Completed,
            Some(_) => ZiskAggregationRangeStatus::InFlight,
            None => ZiskAggregationRangeStatus::Unknown,
        }
    }

    /// Pick the next aggregation job: first re-offer timed-out
    /// assignments, then the oldest formed range.
    pub async fn pick_next_job(&self, prover_id: &str) -> Option<ZiskAggregationJob> {
        let now = Instant::now();
        let mut state = self.state.lock().await;
        state.recover_expired_verifications(now, self.verification_timeout);

        // Return timed-out leases to the pending queue.
        let timed_out: Vec<BatchRange> = state
            .ranges
            .iter()
            .filter(|(_, job)| match job {
                RangeJob::Assigned { lease, .. } => {
                    now.duration_since(lease.since) >= self.assignment_timeout
                }
                _ => false,
            })
            .map(|(&range, _)| range)
            .collect();
        for range in timed_out {
            let Some(RangeJob::Assigned { lease, failures }) = state.ranges.remove(&range) else {
                unreachable!("filtered to assigned ranges");
            };
            tracing::warn!(
                %range,
                old_prover = lease.prover_id,
                "ZiSK aggregation job timed out, returning to queue"
            );
            state.ranges.insert(range, RangeJob::Pending { failures });
        }

        state.form_ready_ranges();

        // Lowest pending range first: that is the order they settle in.
        let range = state
            .ranges_in(|job| matches!(job, RangeJob::Pending { .. }))
            .next()?;
        let Some(RangeJob::Pending { failures }) = state.ranges.remove(&range) else {
            unreachable!("filtered to pending ranges");
        };
        let (from, to) = (range.from(), range.to());
        let streams: Option<Vec<(u64, Vec<u8>)>> = range
            .batches()
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
        let generation = state.issue_generation();
        state.ranges.insert(
            range,
            RangeJob::Assigned {
                lease: Lease {
                    prover_id: prover_id.to_string(),
                    since: now,
                    generation,
                },
                failures,
            },
        );
        tracing::info!(%range, prover_id, "ZiSK aggregation job assigned");
        Some(ZiskAggregationJob {
            from_batch: from,
            to_batch: to,
            streams,
        })
    }

    /// Phase 1: the wire shape of an aggregated-range submission.
    fn check_wire_shape(
        proof: &[u8],
        public_values: &[u8],
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
        Ok(())
    }

    /// Phase 2: the aggregator program-VK tripwire. A drift means the prover
    /// runs a different aggregator guest build.
    fn check_aggregator_vk(
        &self,
        range: BatchRange,
        prover_id: &str,
        public_values: &[u8],
    ) -> Result<B256, ZiskAggregationSubmitError> {
        let reported_vk = B256::from_slice(&public_values[..32]);
        let Some(expected) = self.expected_program_vk else {
            tracing::info!(
                %range,
                %reported_vk,
                "aggregator program VK reported (no expected VK configured)"
            );
            return Ok(reported_vk);
        };
        if reported_vk != expected {
            ZISK_LANE_METRICS.aggregated_vk_drift.inc();
            tracing::error!(
                %range,
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
        Ok(reported_vk)
    }

    /// Phase 3: everything derivable from a range's buffered inputs alone.
    ///
    /// Pure, so the caller can finish borrowing the inputs before it touches
    /// the state. The inner vadcop tripwire checks every input against the VK
    /// set of its OWN protocol version, so an upgrade window with an entry per
    /// version keeps the check armed for both; it mirrors the per-batch lane's.
    fn check_range_inputs(&self, inputs: &[&AggregationInput]) -> RangeCheck {
        if self.multi_proof_mode == MultiProofMode::Required
            && let Some(version) = inputs
                .iter()
                .map(|input| &input.protocol_version)
                .find(|version| !self.expected_inner_vks.contains_key(version))
        {
            return RangeCheck::UnpinnedVersion(version.clone());
        }
        let drift = inputs.iter().find_map(|input| {
            let expected = self
                .expected_inner_vks
                .get(&input.protocol_version)?
                .vadcop_vk;
            (input.vadcop_vk != expected).then_some((input.vadcop_vk, expected))
        });
        match expected_aggregated_public_input(inputs) {
            Err(msg) => RangeCheck::NoDigest(msg),
            Ok(digest) => match drift {
                Some((reported, expected)) => RangeCheck::InnerVadcopDrift { reported, expected },
                None => RangeCheck::Ok { digest },
            },
        }
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
        range: BatchRange,
        mut proof: Vec<u8>,
        mut public_values: Vec<u8>,
        prover_id: &str,
    ) -> Result<(), ZiskAggregationSubmitError> {
        let (from_batch, to_batch) = (range.from(), range.to());
        // 1. Wire shape and the aggregator guest build, both checked before
        //    the lease is touched so a rejection leaves the range assigned to
        //    time out for another prover.
        Self::check_wire_shape(&proof, &public_values)?;
        let reported_vk = self.check_aggregator_vk(range, prover_id, &public_values)?;

        let mut state = self.state.lock().await;
        // Borrow the assignment while validating. It remains intact while
        // native verification capacity is queued, so normal load is governed
        // by the external-prover lease rather than the shorter verification
        // timeout.
        let Some(RangeJob::Assigned { lease, failures }) = state.ranges.get(&range) else {
            return Err(ZiskAggregationSubmitError::UnknownRange {
                from: from_batch,
                to: to_batch,
            });
        };
        let generation = lease.generation;
        let failures = *failures;

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
        // 3. Everything derivable from the buffered inputs alone: the binding
        //    digest, the inner vadcop tripwire, and whether every input's
        //    protocol version is pinned. Pure, so the `inputs` borrow ends
        //    before the state below is touched.
        let expected = match self.check_range_inputs(&inputs) {
            RangeCheck::Ok { digest } => digest,
            RangeCheck::UnpinnedVersion(version) => {
                tracing::error!(
                    %range,
                    %version,
                    "no ZiSK VKs configured for a protocol version in this range — refusing to \
                     accept its aggregated proof. Configure `prover_api.zisk_vks` for the version."
                );
                state.requeue_or_abandon(range, failures, self.multi_proof_mode);
                return Err(ZiskAggregationSubmitError::MissingVersionKeys { version });
            }
            RangeCheck::NoDigest(msg) => {
                if state.requeue_or_abandon(range, failures, self.multi_proof_mode) {
                    ZISK_LANE_METRICS.unprovable.inc();
                    tracing::error!(
                        %range,
                        attempts = MAX_DIGEST_MISMATCH_ATTEMPTS,
                        retained = self.multi_proof_mode == MultiProofMode::Required,
                        "ZiSK aggregation range unprovable: its buffered per-batch inputs \
                         could not form a binding digest on every attempt. Investigate the \
                         divergence. Required keeps the range queued and settlement waits; \
                         Shadow drops it and loses the range's coverage."
                    );
                }
                return Err(ZiskAggregationSubmitError::DigestMismatch(msg));
            }
            RangeCheck::InnerVadcopDrift { reported, expected } => {
                ZISK_LANE_METRICS.aggregated_vadcop_vk_drift.inc();
                tracing::error!(
                    %range,
                    prover_id,
                    %reported,
                    %expected,
                    "aggregated range inner vadcop VK drift — the per-batch inputs were proven \
                     under a different recursive setup than the configured rootCVadcopFinal"
                );
                // Replace the assignment with the same failure transition as
                // a digest mismatch, so the range is not lost but does not
                // loop forever in Shadow.
                if state.requeue_or_abandon(range, failures, self.multi_proof_mode) {
                    ZISK_LANE_METRICS.unprovable.inc();
                }
                return Err(ZiskAggregationSubmitError::InnerVadcopVkDrift { reported, expected });
            }
        };

        let got = B256::from_slice(&public_values[32..64]);
        if got != expected {
            ZISK_LANE_METRICS.aggregated_digest_mismatches.inc();
            if state.requeue_or_abandon(range, failures, self.multi_proof_mode) {
                ZISK_LANE_METRICS.unprovable.inc();
                tracing::error!(
                    from = from_batch,
                    to = to_batch,
                    prover_id,
                    %got,
                    %expected,
                    attempts = MAX_DIGEST_MISMATCH_ATTEMPTS,
                    retained = self.multi_proof_mode == MultiProofMode::Required,
                    "ZiSK aggregation range unprovable: binding digest mismatched on every \
                     attempt (deterministic divergence). One proof system disagrees; \
                     investigate. Required keeps the range queued and settlement waits; \
                     Shadow drops it and loses the range's coverage."
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

        // Checks the wire form and that the public signal is derivable, as the
        // on-chain ZiskVerifier does before the pairing; the pairing itself
        // stays L1-verified.
        //
        // Reserve native verification capacity before starting the shorter
        // verification lease. Cancellation or reassignment while queued leaves
        // this range on its ordinary external-prover assignment.
        drop(state);
        let verification_permit = if self.proof_verification_enabled {
            Some(
                self.verification_slots
                    .clone()
                    .acquire_owned()
                    .await
                    .map_err(|error| {
                        ZiskAggregationSubmitError::ProofVerificationFailed(error.to_string())
                    })?,
            )
        } else {
            None
        };

        // Runs without the state lock, or a verifier's cost would decide how
        // long every pick and status read is serialized. Re-check the captured
        // generation after queueing so stale work never consumes native CPU or
        // overwrites the replacement lease.
        let mut state = self.state.lock().await;
        let failures = Self::assigned_failures_for_generation(&state, range, generation)?;
        state.ranges.insert(
            range,
            RangeJob::Verifying {
                generation,
                since: Instant::now(),
                failures,
            },
        );
        drop(state);
        let verified = if let Some(permit) = verification_permit {
            let ((returned_proof, returned_public_values), verdict) =
                run_blocking_verifier(permit, (proof, public_values), |input| {
                    zisk_verifier::verify_aggregated_range(&input.0, &input.1)
                })
                .await
                .map_err(ZiskAggregationSubmitError::ProofVerificationFailed)?;
            proof = returned_proof;
            public_values = returned_public_values;
            verdict
        } else {
            Ok(())
        };
        let mut state = self.state.lock().await;
        // Anything that retires ranges — settlement, a discard — removes the
        // entry, so a missing or replaced one means this verification is stale.
        // The failure count comes back out of the entry rather than from a
        // local captured before the lock was released, so it is the count this
        // range actually carries.
        if !state.ranges.contains_key(&range) {
            tracing::info!(
                %range,
                "aggregated ZiSK range was retired while its proof was verifying — dropping it"
            );
            return Ok(());
        }
        let failures = Self::verification_failures_for_generation(&state, range, generation)?;
        if to_batch <= state.floor {
            state.ranges.remove(&range);
            // Settled while this was verifying: the range went downstream
            // without this proof and its inputs are gone. Nothing to park and
            // nothing to retry.
            tracing::info!(
                from = from_batch,
                to = to_batch,
                floor = state.floor,
                "aggregated ZiSK range settled while its proof was being verified — dropping it"
            );
            return Ok(());
        }
        if let Err(e) = verified {
            ZISK_LANE_METRICS
                .aggregated_proof_verification_failures
                .inc();
            tracing::error!(
                from = from_batch,
                to = to_batch,
                prover_id,
                "aggregated ZiSK range proof rejected by off-chain verification: {e}"
            );
            // Reject: do not park it. The range is `Verifying`, so replace it
            // with the same failure transition as a digest mismatch.
            if state.requeue_or_abandon(range, failures, self.multi_proof_mode) {
                // The threshold must be as loud here as on a digest mismatch:
                // otherwise a range that fails verification on every attempt
                // either stops existing or waits forever, with nothing to page
                // on.
                ZISK_LANE_METRICS.unprovable.inc();
                tracing::error!(
                    from = from_batch,
                    to = to_batch,
                    attempts = MAX_DIGEST_MISMATCH_ATTEMPTS,
                    retained = self.multi_proof_mode == MultiProofMode::Required,
                    "aggregated ZiSK range proof failed off-chain verification on every attempt"
                );
            }
            return Err(ZiskAggregationSubmitError::ProofVerificationFailed(
                e.to_string(),
            ));
        }

        ZISK_LANE_METRICS.aggregated_proofs_accepted.inc();
        // Completion discards this attempt's failure count, so a later
        // re-keyed range with the same bounds starts clean.

        if self.multi_proof_mode == MultiProofMode::Shadow {
            // Shadow proving composes nothing, so the verification IS the end
            // of the range: retire it here, because no rendezvous will ever take
            // a parked proof. The range keeps its coverage; whatever else the cut
            // drops is counted as lost.
            let late = to_batch <= state.settled_up_to;
            state.retire_up_to(to_batch, RetireCause::Verified { range });
            state.record_gauges();
            drop(state);
            if late {
                ZISK_LANE_METRICS.ranges_verified_after_settlement.inc();
            }
            tracing::info!(
                from = from_batch,
                to = to_batch,
                prover_id,
                aggregator_program_vk = %reported_vk,
                digest = %got,
                late,
                "aggregated ZiSK proof accepted and verified; shadow proving keeps it off L1, \
                 so the range is complete"
            );
            return Ok(());
        }

        tracing::info!(
            from = from_batch,
            to = to_batch,
            prover_id,
            aggregator_program_vk = %reported_vk,
            digest = %got,
            "aggregated ZiSK proof accepted, awaiting Airbender SNARK for multi-proof composition"
        );
        let parked = CompletedAggregatedProof {
            proof,
            public_values,
        };
        let range_ready = self.range_ready.clone();
        state.ranges.insert(range, RangeJob::Completed(parked));
        let mut evicted = Vec::new();
        // Same reason as the retirement above: in `Required` the parked proof is
        // what the range composer is waiting for, so dropping it strands the
        // batches behind it for good.
        while self.multi_proof_mode != MultiProofMode::Required
            && state.count(|job| matches!(job, RangeJob::Completed(_))) > MAX_COMPLETED
        {
            let oldest = state
                .ranges_in(|job| matches!(job, RangeJob::Completed(_)))
                .next()
                .expect("non-empty");
            state.ranges.remove(&oldest);
            evicted.push(oldest);
            tracing::error!(
                range = %oldest,
                "too many aggregated proofs awaiting their Airbender SNARK — dropping the oldest (Airbender lane stuck?)"
            );
        }
        state.record_gauges();
        drop(state);
        // Tell the range proving stage the ZiSK half is ready. A closed or full
        // channel is not an error here: the proof stays parked, and either the
        // Airbender arrival or any already-queued token re-checks this state.
        if let Some(sender) = range_ready {
            let _ = sender.try_send(range);
        }
        Ok(())
    }

    /// Take the validated aggregated proof for a range, if one is parked.
    /// Called by the Airbender SNARK submission path to compose the
    /// MultiProof. On success the consumed batches' inputs and every
    /// tracked range they overlap are retired, and the floor advances.
    pub async fn take_completed(&self, range: BatchRange) -> Option<CompletedAggregatedProof> {
        let taken = {
            let mut state = self.state.lock().await;
            let Some(RangeJob::Completed(taken)) = state.ranges.remove(&range) else {
                return None;
            };
            state.retire_up_to(range.to(), RetireCause::Verified { range });
            state.record_gauges();
            taken
        };
        Some(taken)
    }

    /// Drop aggregation state for batches at or below `batch_to`: called when
    /// those batches were sent downstream and can never join a rendezvous
    /// anymore (a required-mode settlement, or the fake-SNARK pass). Ranges
    /// straddling the cut are dropped whole; inputs above the cut stay for
    /// future re-keyed ranges.
    pub async fn discard_up_to(&self, batch_to: u64) {
        let mut state = self.state.lock().await;
        state.retire_up_to(batch_to, RetireCause::Unverified);
        state.record_gauges();
    }

    /// The batches at or below `batch_to` went downstream on L1.
    ///
    /// - [`MultiProofMode::Required`]: they settled through a composed
    ///   multi-proof (or, on the Airbender-only fallback, without one), so no
    ///   range over them can rendezvous — their state is discarded.
    /// - [`MultiProofMode::Shadow`]: settlement never waits for this lane, so
    ///   the range keeps its inputs and its place in the queue and is still
    ///   proven and verified. Only the settlement watermark moves, which marks
    ///   that verification as late when it lands.
    pub async fn on_batches_settled(&self, batch_to: u64) {
        match self.multi_proof_mode {
            MultiProofMode::Required => self.discard_up_to(batch_to).await,
            MultiProofMode::Shadow => {
                let mut state = self.state.lock().await;
                state.settled_up_to = state.settled_up_to.max(batch_to);
            }
        }
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
pub(crate) fn expected_aggregated_public_input(
    inputs: &[&AggregationInput],
) -> Result<B256, String> {
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
mod tests;
