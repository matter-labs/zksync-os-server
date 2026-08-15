//! Unit tests for the module above; kept in their own file because the
//! manager's logic and its coverage had both outgrown one screenful.

use super::*;
use crate::commitment::ZISK_PUBLIC_VALUES_BYTES;
use crate::test_util::create_test_batch_envelope;
use crate::vadcop_stream::synthetic_stream;
use zksync_os_batch_types::batcher_model::FriProof;
use zksync_os_batch_types::batcher_model::ZISK_SNARK_PROOF_BYTES;
use zksync_os_types::ProvingVersion;

const TEST_PROTOCOL_VERSION: ProtocolSemanticVersion = ProtocolSemanticVersion::new(0, 31, 0);

/// Local re-execution reproduced the submission: two independent ZiSK runs
/// agree against the native result, which is what makes a divergence
/// claim (and the halt it triggers) defensible.
#[test]
fn local_agreeing_with_submission_corroborates_divergence() {
    let (submitted, expected) = (B256::repeat_byte(1), B256::repeat_byte(2));
    assert_eq!(
        classify_mismatch(Some(submitted), submitted, expected),
        MismatchClassification::CorroboratedDivergence
    );
}

/// Local re-execution agrees with the native result, so the submission is
/// simply wrong — a prover problem, never a divergence alarm.
#[test]
fn local_agreeing_with_expected_is_a_wrong_result() {
    let (submitted, expected) = (B256::repeat_byte(1), B256::repeat_byte(2));
    assert_eq!(
        classify_mismatch(Some(expected), submitted, expected),
        MismatchClassification::WrongResult
    );
}

/// Without a local answer, or with three different ones, nothing is
/// established — an inconclusive check must never be read as divergence.
#[test]
fn missing_or_three_way_local_answer_is_inconclusive() {
    let (submitted, expected, other) = (
        B256::repeat_byte(1),
        B256::repeat_byte(2),
        B256::repeat_byte(3),
    );
    assert_eq!(
        classify_mismatch(None, submitted, expected),
        MismatchClassification::Inconclusive
    );
    assert_eq!(
        classify_mismatch(Some(other), submitted, expected),
        MismatchClassification::Inconclusive
    );
}

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
        seal_shadow_commitment: None,
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
        ZiskLaneConfig {
            assignment_timeout: Duration::from_secs(60),
            expected_vks,
            chain_id: TEST_CHAIN_ID,
            chain_config: TEST_CHAIN_CONFIG,
            proof_verification_enabled: true,
        },
        ZiskLaneWiring {
            aggregation_sink: test_sink(1, MultiProofMode::Required),
            mode: lane_mode(MultiProofMode::Required),
            halt_on_mismatch: None,
        },
    )
}

/// Tests still speak in `MultiProofMode`; the lane wants the mode with the
/// channel it needs, and `Required` only needs a live sender for the tests
/// that read it.
fn lane_mode(mode: MultiProofMode) -> ZiskLaneMode {
    match mode {
        MultiProofMode::Shadow => ZiskLaneMode::Shadow,
        MultiProofMode::Required => {
            let (batch_ready, mut ready) = tokio::sync::mpsc::channel(64);
            tokio::spawn(async move { while ready.recv().await.is_some() {} });
            ZiskLaneMode::Required { batch_ready }
        }
    }
}

fn aggregation_mode(mode: MultiProofMode) -> crate::aggregation_job_manager::ZiskAggregationMode {
    use crate::aggregation_job_manager::ZiskAggregationMode;
    match mode {
        MultiProofMode::Shadow => ZiskAggregationMode::Shadow,
        MultiProofMode::Required => ZiskAggregationMode::Required {
            range_ready: tokio::sync::mpsc::channel(16).0,
        },
    }
}

/// An aggregation manager for fixtures that only need the per-batch lane to
/// have somewhere to hand accepted streams.
fn test_sink(
    range_size: usize,
    multi_proof_mode: MultiProofMode,
) -> std::sync::Arc<crate::aggregation_job_manager::ZiskAggregationJobManager> {
    std::sync::Arc::new(
        crate::aggregation_job_manager::ZiskAggregationJobManager::new(
            crate::aggregation_job_manager::ZiskAggregationLaneConfig {
                range_size,
                assignment_timeout: Duration::from_secs(60),
                verification_timeout: Duration::from_secs(60),
                expected_program_vk: None,
                expected_inner_vks: HashMap::new(),
                proof_verification_enabled: false,
                mode: aggregation_mode(multi_proof_mode),
            },
        ),
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
    manager_with_sink_of_range(expected_vk, multi_proof_mode, 1)
}

/// `range_size` picks how many batches an aggregation range takes. A range
/// wider than the input buffer keeps every input buffered, which is how the
/// buffer-full path is reached.
fn manager_with_sink_of_range(
    expected_vk: Option<B256>,
    multi_proof_mode: MultiProofMode,
    range_size: usize,
) -> (
    ZiskJobManager,
    std::sync::Arc<crate::aggregation_job_manager::ZiskAggregationJobManager>,
) {
    manager_with_sink_armed_of_range(expected_vk, multi_proof_mode, range_size, None)
}

/// The same fixture with halt-on-mismatch armed.
fn manager_with_sink_armed(
    expected_vk: Option<B256>,
    multi_proof_mode: MultiProofMode,
    halt: Option<tokio::sync::oneshot::Sender<String>>,
) -> (
    ZiskJobManager,
    std::sync::Arc<crate::aggregation_job_manager::ZiskAggregationJobManager>,
) {
    manager_with_sink_armed_of_range(expected_vk, multi_proof_mode, 1, halt)
}

fn manager_with_sink_armed_of_range(
    expected_vk: Option<B256>,
    multi_proof_mode: MultiProofMode,
    range_size: usize,
    halt: Option<tokio::sync::oneshot::Sender<String>>,
) -> (
    ZiskJobManager,
    std::sync::Arc<crate::aggregation_job_manager::ZiskAggregationJobManager>,
) {
    // Required refuses a submission whose protocol version has no pinned
    // keys, so the fixture always pins one. `None` pins exactly what the
    // synthetic streams report, which is the "keys agree" case; `Some`
    // pins a different program VK to exercise the drift tripwire.
    let expected_vks = HashMap::from([(
        TEST_PROTOCOL_VERSION,
        ZiskVkSet {
            program_vk: expected_vk.unwrap_or_else(|| vk_bytes([1, 2, 3, 4])),
            // The stream fixtures carry these vadcop limbs.
            vadcop_vk: vk_bytes([5, 6, 7, 8]),
        },
    )]);
    // Dependency order, as the server builds it: the sink first, then the
    // lane that feeds it.
    let agg = std::sync::Arc::new(
        crate::aggregation_job_manager::ZiskAggregationJobManager::new(
            crate::aggregation_job_manager::ZiskAggregationLaneConfig {
                range_size,
                assignment_timeout: Duration::from_secs(60),
                verification_timeout: Duration::from_secs(60),
                expected_program_vk: None,
                expected_inner_vks: HashMap::new(),
                proof_verification_enabled: false,
                mode: aggregation_mode(multi_proof_mode),
            },
        ),
    );
    let manager = ZiskJobManager::new(
        ZiskLaneConfig {
            assignment_timeout: Duration::from_secs(60),
            expected_vks,
            chain_id: TEST_CHAIN_ID,
            chain_config: TEST_CHAIN_CONFIG,
            proof_verification_enabled: false,
        },
        ZiskLaneWiring {
            aggregation_sink: agg.clone(),
            mode: lane_mode(multi_proof_mode),
            halt_on_mismatch: halt,
        },
    );
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
async fn acceptance_releases_the_batch_to_the_aggregation_lane() {
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
    // The aggregation lane owns the stream from here, so this lane keeps
    // nothing for the batch and its slot is free immediately.
    assert_eq!(manager.batch_status(7).await, ZiskBatchStatus::Unknown);
    assert!(!manager.has_pending_jobs().await);
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
    // Accepted and handed on: a re-add opens a fresh job, because this
    // lane no longer holds anything for the batch.
    manager
        .submit_proof(7, stream, vec![], "prover-1")
        .await
        .expect("accepted");
    manager.add_job(7, job_data(7, vec![0xCD; 8])).await;
    assert_eq!(manager.batch_status(7).await, ZiskBatchStatus::InFlight);
}

/// Pick-timeout reassignment. The assignment deadline is this crate's
/// liveness mechanic: a prover that vanishes must not strand a batch, so
/// the job returns to pending and the next prover receives the same
/// prover input.
#[tokio::test]
async fn pick_timeout_reassigns_job() {
    // A zero timeout makes every assignment reassignable at the next pick.
    let manager = ZiskJobManager::new(
        ZiskLaneConfig {
            assignment_timeout: Duration::ZERO,
            expected_vks: HashMap::new(),
            chain_id: TEST_CHAIN_ID,
            chain_config: TEST_CHAIN_CONFIG,
            proof_verification_enabled: true,
        },
        ZiskLaneWiring {
            aggregation_sink: test_sink(1, MultiProofMode::Required),
            mode: lane_mode(MultiProofMode::Required),
            halt_on_mismatch: None,
        },
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

/// A mismatching submission that this server did not cryptographically
/// verify must never reach the halt channel: `submit` is reachable by
/// anyone who can call the prover API, so otherwise a well-formed stream
/// with a wrong commitment would be a kill switch. The job is requeued
/// instead, and the halt stays armed for real evidence.
#[tokio::test]
async fn unverified_commitment_mismatch_never_halts() {
    // `manager_with_sink` builds the manager with proof verification off,
    // so nothing submitted here counts as verified.
    let (halt_tx, mut halt_rx) = tokio::sync::oneshot::channel();
    let (manager, _agg) = manager_with_sink_armed(None, MultiProofMode::Required, Some(halt_tx));

    manager.add_job(7, job_data(7, vec![0xAB; 32])).await;
    manager
        .pick_next_job("prover-1")
        .await
        .expect("job available");

    let err = manager
        .submit_proof(7, mismatching_vadcop_stream(), vec![], "prover-1")
        .await
        .expect_err("mismatch must be rejected");
    assert!(matches!(err, ZiskSubmitError::CommitmentMismatch));

    assert!(
        matches!(
            halt_rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ),
        "an unverified submission must not be able to halt the node"
    );
    assert!(
        manager.has_pending_jobs().await,
        "the job is requeued for another prover instead"
    );
}

/// The halt decision itself: only a verified proof whose result local
/// re-execution reproduced justifies stopping the node.
#[test]
fn only_verified_and_corroborated_mismatches_halt() {
    assert!(should_halt(
        true,
        MismatchClassification::CorroboratedDivergence
    ));
    assert!(
        !should_halt(false, MismatchClassification::CorroboratedDivergence),
        "unverified bytes must never halt, however they classify"
    );
    assert!(!should_halt(true, MismatchClassification::WrongResult));
    assert!(!should_halt(true, MismatchClassification::Inconclusive));
}

/// In `Shadow` a batch's ZiSK coverage is sheddable: settlement never waits
/// for it. A persistent (deterministic) commitment mismatch is therefore
/// given up on after `MAX_COMMITMENT_MISMATCH_ATTEMPTS` instead of
/// requeuing forever — the job is dropped (slot freed), no state leaks,
/// and the manager keeps serving other batches.
#[tokio::test]
async fn persistent_mismatch_gives_up_and_frees_slot() {
    // Shadow, no halt armed: sequencing does not wait on this lane.
    let (manager, _agg) = manager_with_sink(None, MultiProofMode::Shadow);
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
    assert_eq!(manager.batch_status(8).await, ZiskBatchStatus::Unknown);
}

/// `Required` is the opposite contract: the commit gate holds the batch
/// until this lane proves it, and the gate entry is released by nothing but
/// a proof. Dropping the job would strand the batch behind an entry no
/// proof can ever clear — block production stops for good, with no job left
/// for a repaired prover to pick up. So the job is kept and re-alarmed.
#[tokio::test]
async fn required_mode_never_abandons_a_mismatching_batch() {
    let (manager, _agg) = manager_with_sink(None, MultiProofMode::Required);
    manager.add_job(7, job_data(7, vec![0xAB; 32])).await;

    // Well past the give-up threshold that `Shadow` would have hit.
    for _ in 0..(MAX_COMMITMENT_MISMATCH_ATTEMPTS * 3) {
        manager
            .pick_next_job("prover-1")
            .await
            .expect("the job stays available for retry");
        let err = manager
            .submit_proof(7, mismatching_vadcop_stream(), vec![], "prover-1")
            .await
            .expect_err("mismatch must be rejected");
        assert!(matches!(err, ZiskSubmitError::CommitmentMismatch));
    }
    assert_eq!(
        manager.batch_status(7).await,
        ZiskBatchStatus::InFlight,
        "a batch that gates settlement is never abandoned"
    );

    // And a repaired prover still lands it.
    let good = job_data(7, vec![0xAB; 32]);
    let stream = matching_vadcop_stream(&good);
    manager.pick_next_job("prover-2").await.expect("job");
    manager
        .submit_proof(7, stream, vec![], "prover-2")
        .await
        .expect("the batch proves once a working prover picks it up");
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
    assert_eq!(manager.batch_status(7).await, ZiskBatchStatus::Unknown);
}

/// Aggregated mode: an accepted `vadcop_final` stream is buffered in
/// the aggregation manager as range input AND parked here as a
/// mode-tagged completion marker; the aggregation job carries the
/// stream once its SNARK range is noted.
#[tokio::test]
async fn aggregated_mode_accepts_stream_and_feeds_sink() {
    let (manager, agg) = manager_with_sink(None, MultiProofMode::Required);

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

    assert_eq!(
        manager.batch_status(7).await,
        ZiskBatchStatus::Unknown,
        "acceptance hands the batch to the aggregation lane and frees the slot"
    );
    assert!(agg.has_input(7).await, "stream buffered as range input");

    agg.note_snark_range(crate::BatchRange::of(7, 7)).await;
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
    let (manager, _agg) = manager_with_sink(None, MultiProofMode::Required);

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
    let (manager, agg) = manager_with_sink(None, MultiProofMode::Required);

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
    agg.note_snark_range(crate::BatchRange::of(7, 7)).await;

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
    let (manager, _agg) = manager_with_sink(Some(vk_bytes([1, 2, 3, 4])), MultiProofMode::Required);

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
        ZiskBatchStatus::Unknown,
        "the corrected proof is accepted and handed to the aggregation lane"
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
        ZiskLaneConfig {
            assignment_timeout: Duration::from_secs(60),
            expected_vks: HashMap::from([(
                TEST_PROTOCOL_VERSION,
                ZiskVkSet {
                    program_vk: vk_bytes([1, 2, 3, 4]),
                    vadcop_vk: vk_bytes(expected_vadcop_limbs),
                },
            )]),
            chain_id: TEST_CHAIN_ID,
            chain_config: TEST_CHAIN_CONFIG,
            proof_verification_enabled: false,
        },
        ZiskLaneWiring {
            aggregation_sink: test_sink(1, MultiProofMode::Required),
            mode: lane_mode(MultiProofMode::Required),
            halt_on_mismatch: None,
        },
    );
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
    assert_eq!(manager.batch_status(7).await, ZiskBatchStatus::Unknown);
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
    // program VK is exercised. V32 has no entry: in Required its batches
    // are refused rather than waved through.
    let manager = ZiskJobManager::new(
        ZiskLaneConfig {
            assignment_timeout: Duration::from_secs(60),
            expected_vks: HashMap::from([
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
            chain_id: TEST_CHAIN_ID,
            chain_config: TEST_CHAIN_CONFIG,
            proof_verification_enabled: false,
        },
        ZiskLaneWiring {
            aggregation_sink: test_sink(1, MultiProofMode::Required),
            mode: lane_mode(MultiProofMode::Required),
            halt_on_mismatch: None,
        },
    );

    // A v30 batch proven under the v30 key is accepted.
    let d30 = job_data_versioned(1, vec![0xAB; 8], V30);
    let s30 = synthetic_stream(limbs_v30, [5, 6, 7, 8], expected_commitment(&d30).0);
    manager.add_job(1, d30).await;
    manager.pick_next_job("p").await.expect("job 1");
    manager
        .submit_proof(1, s30, vec![], "p")
        .await
        .expect("v30 key accepted for a v30 batch");
    assert_eq!(manager.batch_status(1).await, ZiskBatchStatus::Unknown);

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
    assert_eq!(manager.batch_status(2).await, ZiskBatchStatus::Unknown);

    // A v32 batch has no configured entry. Its proof would go on L1
    // unchecked against any pinned guest build, so Required refuses it —
    // an upgrade brings versions startup validation could not know about,
    // which is why the check has to be here and not only at startup. The
    // job survives the rejection and the batch stays gated.
    let d32 = job_data_versioned(3, vec![0xEE; 8], V32);
    let s32 = synthetic_stream(
        [0xAA, 0xAA, 0xAA, 0xAA],
        [5, 6, 7, 8],
        expected_commitment(&d32).0,
    );
    manager.add_job(3, d32).await;
    manager.pick_next_job("p").await.expect("job 3");
    let err = manager
        .submit_proof(3, s32, vec![], "p")
        .await
        .expect_err("an unpinned protocol version is refused in Required");
    assert!(
        matches!(err, ZiskSubmitError::MissingVersionKeys { .. }),
        "{err}"
    );
    assert_eq!(
        manager.batch_status(3).await,
        ZiskBatchStatus::InFlight,
        "the rejection must not consume the job"
    );
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

/// A per-batch proof rejected because the aggregation buffer is full parks
/// in the backlog and gives its active slot back. Nothing else can wake it:
/// the commit gate stops admitting batches exactly while it waits, so the
/// release of aggregation capacity must promote it on its own.
#[tokio::test]
async fn releasing_aggregation_capacity_wakes_a_parked_job() {
    use crate::aggregation_job_manager::MAX_BUFFERED_INPUTS;

    // A range wider than the buffer never forms, so inputs accumulate.
    let (manager, _agg) =
        manager_with_sink_of_range(None, MultiProofMode::Required, MAX_BUFFERED_INPUTS * 2);
    for batch in 1..=MAX_BUFFERED_INPUTS as u64 {
        let data = job_data(batch, vec![batch as u8; 8]);
        let stream = matching_vadcop_stream(&data);
        manager.add_job(batch, data).await;
        manager.pick_next_job("prover-1").await.expect("job");
        manager
            .submit_proof(batch, stream, vec![], "prover-1")
            .await
            .expect("proof accepted while the buffer has room");
    }

    // One more: accepted by this lane but refused by the full buffer.
    let overflow = MAX_BUFFERED_INPUTS as u64 + 1;
    let data = job_data(overflow, vec![0xEE; 8]);
    let stream = matching_vadcop_stream(&data);
    manager.add_job(overflow, data).await;
    manager.pick_next_job("prover-1").await.expect("job");
    manager
        .submit_proof(overflow, stream, vec![], "prover-1")
        .await
        .expect("a buffer-full rejection is not an error");
    assert!(
        manager.pick_next_job("prover-1").await.is_none(),
        "the re-parked job waits in the backlog, not in the active queue"
    );

    // Settling early batches releases buffered inputs. The parked job must
    // become pickable without any new batch arriving.
    manager.on_batches_settled(10).await;
    let woken = manager
        .pick_next_job("prover-1")
        .await
        .expect("released aggregation capacity must promote the parked job");
    assert_eq!(woken.batch_number, overflow);
}

/// The commit gate gets exactly one notice per batch and never polls, so a
/// notice that does not fit in the channel would leave its batch parked until a
/// restart. A full channel must therefore delay the submission, not drop the
/// message.
#[tokio::test]
async fn a_full_ready_channel_delays_the_notice_rather_than_dropping_it() {
    let (ready_tx, mut ready_rx) = tokio::sync::mpsc::channel(1);
    let agg = test_sink(1, MultiProofMode::Required);
    let manager = std::sync::Arc::new(ZiskJobManager::new(
        ZiskLaneConfig {
            assignment_timeout: Duration::from_secs(60),
            expected_vks: HashMap::from([(
                TEST_PROTOCOL_VERSION,
                ZiskVkSet {
                    program_vk: vk_bytes([1, 2, 3, 4]),
                    vadcop_vk: vk_bytes([5, 6, 7, 8]),
                },
            )]),
            chain_id: TEST_CHAIN_ID,
            chain_config: TEST_CHAIN_CONFIG,
            proof_verification_enabled: false,
        },
        ZiskLaneWiring {
            aggregation_sink: agg,
            mode: ZiskLaneMode::Required {
                batch_ready: ready_tx.clone(),
            },
            halt_on_mismatch: None,
        },
    ));

    // Occupy the only slot, so the notice below cannot be delivered yet.
    ready_tx.send(u64::MAX).await.expect("the slot starts free");

    let data = job_data(7, vec![0xAB; 32]);
    let stream = matching_vadcop_stream(&data);
    manager.add_job(7, data).await;
    manager.pick_next_job("prover-1").await.expect("job");

    let submitting = tokio::spawn({
        let manager = manager.clone();
        async move { manager.submit_proof(7, stream, vec![], "prover-1").await }
    });

    // The submission must still be in flight: its notice has nowhere to go
    // yet. A dropped notice would let it finish here, which is the regression.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !submitting.is_finished(),
        "the notice must wait for room rather than be dropped"
    );

    // Drain the filler; the batch's own notice must follow it.
    assert_eq!(ready_rx.recv().await, Some(u64::MAX));
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), ready_rx.recv())
            .await
            .expect("the notice must arrive once there is room"),
        Some(7)
    );
    submitting
        .await
        .expect("the submission task")
        .expect("the proof is accepted");
}

/// Cancelling a submission while it waits for commit-gate capacity must not
/// consume the job. The same validated proof can be submitted again once the
/// gate drains; replay is only the process-restart backstop, not the recovery
/// mechanism for an interrupted live request.
#[tokio::test]
async fn a_cancelled_ready_notice_keeps_the_job_retryable() {
    let (ready_tx, mut ready_rx) = tokio::sync::mpsc::channel(1);
    let manager = std::sync::Arc::new(ZiskJobManager::new(
        ZiskLaneConfig {
            assignment_timeout: Duration::from_secs(60),
            expected_vks: HashMap::from([(
                TEST_PROTOCOL_VERSION,
                ZiskVkSet {
                    program_vk: vk_bytes([1, 2, 3, 4]),
                    vadcop_vk: vk_bytes([5, 6, 7, 8]),
                },
            )]),
            chain_id: TEST_CHAIN_ID,
            chain_config: TEST_CHAIN_CONFIG,
            proof_verification_enabled: false,
        },
        ZiskLaneWiring {
            aggregation_sink: test_sink(1, MultiProofMode::Required),
            mode: ZiskLaneMode::Required {
                batch_ready: ready_tx.clone(),
            },
            halt_on_mismatch: None,
        },
    ));

    ready_tx.send(u64::MAX).await.expect("the slot starts free");
    let data = job_data(7, vec![0xAB; 32]);
    let stream = matching_vadcop_stream(&data);
    manager.add_job(7, data).await;
    manager.pick_next_job("prover-1").await.expect("job");

    let submitting = tokio::spawn({
        let manager = manager.clone();
        let stream = stream.clone();
        async move { manager.submit_proof(7, stream, vec![], "prover-1").await }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!submitting.is_finished(), "submission must wait for room");

    submitting.abort();
    assert!(
        submitting
            .await
            .expect_err("task was aborted")
            .is_cancelled(),
        "the request future must have been cancelled"
    );
    assert_eq!(
        manager.batch_status(7).await,
        ZiskBatchStatus::InFlight,
        "cancellation before readiness publication must leave the lease recoverable"
    );

    assert_eq!(ready_rx.recv().await, Some(u64::MAX));
    manager
        .submit_proof(7, stream, vec![], "prover-1")
        .await
        .expect("the interrupted proof can be submitted again");
    assert_eq!(ready_rx.recv().await, Some(7));
    assert_eq!(manager.batch_status(7).await, ZiskBatchStatus::Unknown);
}

/// A submission that started under an older assignment may finish its
/// idempotent aggregation handoff, but it must neither consume the replacement
/// lease nor publish readiness for it.
#[tokio::test]
async fn a_superseded_submission_cannot_complete_a_newer_lease() {
    let superseded_before = ZISK_LANE_METRICS.superseded_submissions.get();
    let (ready_tx, mut ready_rx) = tokio::sync::mpsc::channel(1);
    let manager = std::sync::Arc::new(ZiskJobManager::new(
        ZiskLaneConfig {
            assignment_timeout: Duration::from_millis(10),
            expected_vks: HashMap::from([(
                TEST_PROTOCOL_VERSION,
                ZiskVkSet {
                    program_vk: vk_bytes([1, 2, 3, 4]),
                    vadcop_vk: vk_bytes([5, 6, 7, 8]),
                },
            )]),
            chain_id: TEST_CHAIN_ID,
            chain_config: TEST_CHAIN_CONFIG,
            proof_verification_enabled: false,
        },
        ZiskLaneWiring {
            aggregation_sink: test_sink(1, MultiProofMode::Required),
            mode: ZiskLaneMode::Required {
                batch_ready: ready_tx.clone(),
            },
            halt_on_mismatch: None,
        },
    ));

    ready_tx.send(u64::MAX).await.expect("the slot starts free");
    let data = job_data(7, vec![0xAB; 32]);
    let stream = matching_vadcop_stream(&data);
    manager.add_job(7, data).await;
    manager
        .pick_next_job("prover-1")
        .await
        .expect("first lease");

    let old_submission = tokio::spawn({
        let manager = manager.clone();
        let stream = stream.clone();
        async move { manager.submit_proof(7, stream, vec![], "prover-1").await }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !old_submission.is_finished(),
        "old lease waits for gate room"
    );
    manager
        .pick_next_job("prover-2")
        .await
        .expect("expired lease is reassigned");

    assert_eq!(ready_rx.recv().await, Some(u64::MAX));
    assert!(matches!(
        old_submission.await.expect("submission task"),
        Err(ZiskSubmitError::Superseded(7))
    ));
    assert!(matches!(
        ready_rx.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
    assert_eq!(manager.batch_status(7).await, ZiskBatchStatus::InFlight);

    manager
        .submit_proof(7, stream, vec![], "prover-2")
        .await
        .expect("current lease completes");
    assert_eq!(ready_rx.recv().await, Some(7));
    assert_eq!(
        ZISK_LANE_METRICS.superseded_submissions.get() - superseded_before,
        1
    );
}
