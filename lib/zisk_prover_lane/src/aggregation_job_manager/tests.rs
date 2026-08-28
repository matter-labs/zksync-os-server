//! Unit tests for the module above; kept in their own file because the
//! manager's logic and its coverage had both outgrown one screenful.

use super::*;

const K: usize = 4;
const TEST_PROTOCOL_VERSION: ProtocolSemanticVersion = ProtocolSemanticVersion::new(0, 31, 0);

/// Tests still speak in `MultiProofMode`; the lane now wants the mode with its
/// channel, and `Required` only needs a live sender for the tests that read it.
fn aggregation_mode(mode: MultiProofMode) -> ZiskAggregationMode {
    match mode {
        MultiProofMode::Shadow => ZiskAggregationMode::Shadow,
        MultiProofMode::Required => ZiskAggregationMode::Required {
            range_ready: tokio::sync::mpsc::channel(16).0,
        },
    }
}

fn manager(multi_proof_mode: MultiProofMode) -> ZiskAggregationJobManager {
    manager_with_verification_timeout(multi_proof_mode, Duration::from_secs(60))
}

fn manager_with_verification_timeout(
    multi_proof_mode: MultiProofMode,
    verification_timeout: Duration,
) -> ZiskAggregationJobManager {
    // The submitted aggregated-range proofs are well-shaped SNARK
    // artifacts, which pass the wire-form verification, so proof
    // verification stays on here.
    // Aggregation refuses a protocol version whose keys are not compiled, so
    // the fixture pins the one its inputs carry.
    ZiskAggregationJobManager::new(ZiskAggregationLaneConfig {
        range_size: K,
        assignment_timeout: Duration::from_secs(60),
        verification_timeout,
        expected_program_vks: HashMap::from([(TEST_PROTOCOL_VERSION, B256::ZERO)]),
        expected_inner_vks: HashMap::from([(
            TEST_PROTOCOL_VERSION,
            ZiskVkSet {
                program_vk: B256::repeat_byte(0xA1),
                vadcop_vk: B256::repeat_byte(0xB2),
            },
        )]),
        proof_verification_enabled: true,
        mode: aggregation_mode(multi_proof_mode),
    })
}

/// A buffered input with the given commitment byte pattern and the
/// shared test VKs. The stream payload is a small marker — this
/// manager treats streams as opaque bytes (shape validation happens in
/// `ZiskJobManager` before buffering).
fn input(commitment_byte: u8) -> AggregationInput {
    AggregationInput {
        stream: vec![commitment_byte; 64],
        protocol_version: TEST_PROTOCOL_VERSION,
        program_vk: B256::repeat_byte(0xA1),
        vadcop_vk: B256::repeat_byte(0xB2),
        commitment: B256::repeat_byte(commitment_byte),
    }
}

/// The same input, tagged with another batch protocol version.
fn input_of_version(
    commitment_byte: u8,
    protocol_version: ProtocolSemanticVersion,
) -> AggregationInput {
    AggregationInput {
        protocol_version,
        ..input(commitment_byte)
    }
}

async fn feed(manager: &ZiskAggregationJobManager, batch: u64) {
    manager.on_proof_completed(batch, input(batch as u8)).await;
}

#[tokio::test]
async fn pick_filters_aggregation_ranges_before_leasing() {
    let manager = manager(MultiProofMode::Required);
    manager.note_snark_range(BatchRange::of(1, 4)).await;
    for batch in 1..=4 {
        feed(&manager, batch).await;
    }

    assert!(
        manager
            .pick_next_job_with_capabilities("wrong-agg", Some(&[B256::repeat_byte(0xEE)]))
            .await
            .is_none()
    );

    let supported = crate::ZiskProvingVersion::V1.verification_key_hash();
    let job = manager
        .pick_next_job_with_capabilities("right-agg", Some(&[supported]))
        .await
        .expect("the incompatible request must leave the range pending");
    assert_eq!(job.vk_hash, supported.to_string());
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

/// THE cross-stack binding vector (real 4-batch aggregation session).
/// The aggregator guest's `cross_stack_binding_vector` test and
/// `zksync-os-zisk/guest-aggregator/BINDING_VECTOR.md` pin the same values.
/// Update all pins together.
#[test]
fn binding_digest_matches_cross_stack_vector() {
    let program_vk: B256 = "0x8168c5d383a50a9c7a40561b82bf679cc6dfdab0308417b4fea653362d78d080"
        .parse()
        .unwrap();
    let vadcop_vk: B256 = "0xcf2a309856f107b143836ada112806da71ae11567fa3f2d2050baba5381c7b7d"
        .parse()
        .unwrap();
    let commitments = [
        "0x63c7606faee0ee9eff230fec391e64c0c82a0277947973ce7f6f1c9088c821dd",
        "0x7d6a5ed6ffda210164c11dd6f6fccbd35c4ff70632e845a5bf256e3ec48940b9",
        "0xd5a7b4485d1aece18348655132e73c86b23fa0f251adb173f80123d05a914f15",
        "0xc5ed165443011bac65df4d0f4240de3429c033996e9fce630a631e117537cd61",
    ];
    let inputs: Vec<AggregationInput> = commitments
        .iter()
        .map(|c| AggregationInput {
            stream: vec![],
            protocol_version: TEST_PROTOCOL_VERSION,
            program_vk,
            vadcop_vk,
            commitment: c.parse().unwrap(),
        })
        .collect();
    let refs: Vec<&AggregationInput> = inputs.iter().collect();
    let digest = expected_aggregated_public_input(&refs).unwrap();
    assert_eq!(
        format!("{digest:#x}"),
        "0xf29341c341f2622ba86a21bbb36dde9742e1983e531c278fd1cee04c6f823e2c"
    );
}

/// A single-batch range takes the one public input verbatim and performs no
/// keccak over it, which is the settlement layer's own special case.
#[test]
fn single_batch_digest_folds_the_only_public_input_unhashed() {
    let a = input(0x11);
    let digest = expected_aggregated_public_input(&[&a]).unwrap();
    let mut binding = [0u8; 96];
    binding[..32].copy_from_slice(a.program_vk.as_slice());
    binding[32..64].copy_from_slice(a.vadcop_vk.as_slice());
    binding[64..].copy_from_slice(shr32(&a.commitment).as_slice());
    assert_eq!(digest, keccak256(binding));
}

/// Two batches are the smallest range that folds, and the point where a
/// per-input truncation would first show: the concatenation carries both
/// commitments in full and the shift lands only on the folded result.
#[test]
fn multi_batch_digest_folds_untruncated_public_inputs() {
    let a = input(0x11);
    let b = input(0x22);
    let digest = expected_aggregated_public_input(&[&a, &b]).unwrap();

    let mut preimage = [0u8; 64];
    preimage[..32].copy_from_slice(a.commitment.as_slice());
    preimage[32..].copy_from_slice(b.commitment.as_slice());
    let mut binding = [0u8; 96];
    binding[..32].copy_from_slice(a.program_vk.as_slice());
    binding[32..64].copy_from_slice(a.vadcop_vk.as_slice());
    binding[64..].copy_from_slice(shr32(&keccak256(preimage)).as_slice());
    assert_eq!(digest, keccak256(binding));
}

/// The upgrade-window seam of the inner vadcop tripwire: with an entry per
/// protocol version, a range of EITHER version whose buffered inputs carry
/// a foreign vadcop VK is rejected before the digest comparison, and a range
/// whose version has no compiled entry is never leased. Inputs carry vadcop VK
/// 0xB2 (see `input`), which matches neither configured entry.
#[tokio::test]
async fn inner_vadcop_vk_drift_rejects_range_of_either_version() {
    const NEXT_PROTOCOL_VERSION: ProtocolSemanticVersion = ProtocolSemanticVersion::new(0, 31, 1);
    const UNMAPPED_PROTOCOL_VERSION: ProtocolSemanticVersion =
        ProtocolSemanticVersion::new(0, 32, 0);
    // Both versions are configured throughout; a rejected range is
    // requeued, so each case runs on its own manager to keep the picked
    // range unambiguous.
    let manager_with_both_versions = || {
        let vk_set = |vadcop_byte: u8| ZiskVkSet {
            program_vk: B256::repeat_byte(0xA1),
            vadcop_vk: B256::repeat_byte(vadcop_byte),
        };
        ZiskAggregationJobManager::new(ZiskAggregationLaneConfig {
            range_size: K,
            assignment_timeout: Duration::from_secs(60),
            verification_timeout: Duration::from_secs(60),
            expected_program_vks: HashMap::from([
                (TEST_PROTOCOL_VERSION, B256::ZERO),
                (NEXT_PROTOCOL_VERSION, B256::ZERO),
            ]),
            expected_inner_vks: HashMap::from([
                (TEST_PROTOCOL_VERSION, vk_set(0xCC)),
                (NEXT_PROTOCOL_VERSION, vk_set(0xDD)),
            ]),
            proof_verification_enabled: true,
            mode: aggregation_mode(MultiProofMode::Required),
        })
    };

    for version in [TEST_PROTOCOL_VERSION, NEXT_PROTOCOL_VERSION] {
        let manager = manager_with_both_versions();
        manager.note_snark_range(BatchRange::of(1, 4)).await;
        for batch in 1..=4u64 {
            manager
                .on_proof_completed(batch, input_of_version(batch as u8, version.clone()))
                .await;
        }
        manager.pick_next_job("agg-1").await.expect("range 1..4");
        let err = manager
            .submit_proof(
                BatchRange::of(1, 4),
                vec![0; ZISK_SNARK_PROOF_BYTES],
                aggregated_pv(B256::ZERO),
                "agg-1",
            )
            .await
            .expect_err("inner vadcop VK drift rejected");
        assert!(
            matches!(err, ZiskAggregationSubmitError::InnerVadcopVkDrift { .. }),
            "version {version}: {err}"
        );
    }

    // A range whose protocol version has no compiled manifest cannot be
    // checked against an L1 identity, so it remains pending without a lease.
    let manager = manager_with_both_versions();
    manager.note_snark_range(BatchRange::of(1, 4)).await;
    for batch in 1..=4u64 {
        manager
            .on_proof_completed(
                batch,
                input_of_version(batch as u8, UNMAPPED_PROTOCOL_VERSION),
            )
            .await;
    }
    assert!(manager.pick_next_job("agg-1").await.is_none());
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
    let manager = manager(MultiProofMode::Required);
    for batch in [9u64, 7, 10, 8] {
        feed(&manager, batch).await;
    }
    assert!(
        manager.pick_next_job("agg-1").await.is_none(),
        "inputs without a noted SNARK range must not form a job"
    );

    manager.note_snark_range(BatchRange::of(7, 10)).await;
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
    let manager = manager(MultiProofMode::Required);
    manager.note_snark_range(BatchRange::of(1, 4)).await;
    for batch in [1u64, 2, 4] {
        feed(&manager, batch).await;
    }
    assert_eq!(
        manager.range_status(BatchRange::of(1, 4)).await,
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
    let manager = manager(MultiProofMode::Required);
    manager.note_snark_range(BatchRange::of(5, 8)).await;
    for batch in 5..=8u64 {
        feed(&manager, batch).await;
    }
    let job = manager.pick_next_job("agg-1").await.expect("range 5..8");
    let digest = expected_digest(&manager, 5, 8).await;
    manager
        .submit_proof(
            BatchRange::of(5, 8),
            vec![0; ZISK_SNARK_PROOF_BYTES],
            aggregated_pv(digest),
            "agg-1",
        )
        .await
        .expect("valid aggregated proof accepted");
    assert_eq!((job.from_batch, job.to_batch), (5, 8));
    assert_eq!(
        manager.range_status(BatchRange::of(5, 8)).await,
        ZiskAggregationRangeStatus::Completed
    );

    let taken = manager
        .take_completed(BatchRange::of(5, 8))
        .await
        .expect("parked proof taken");
    assert_eq!(taken.proof.len(), ZISK_SNARK_PROOF_BYTES);
    assert!(
        manager.take_completed(BatchRange::of(5, 8)).await.is_none(),
        "taken exactly once"
    );
    assert_eq!(
        manager.range_status(BatchRange::of(5, 8)).await,
        ZiskAggregationRangeStatus::Unknown
    );

    // Late arrival below the floor is dropped; the next range works.
    feed(&manager, 4).await;
    assert!(!manager.has_input(4).await);
    manager.note_snark_range(BatchRange::of(9, 12)).await;
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
    let manager = manager(MultiProofMode::Required);
    manager.note_snark_range(BatchRange::of(1, 2)).await;
    manager.note_snark_range(BatchRange::of(1, 4)).await;
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
            BatchRange::of(1, 2),
            vec![0; ZISK_SNARK_PROOF_BYTES],
            aggregated_pv(digest),
            "agg-1",
        )
        .await
        .expect("accepted");
    manager
        .take_completed(BatchRange::of(1, 2))
        .await
        .expect("composed");
    // The overlapping (1,4) assignment is retired with the take.
    let digest = B256::ZERO;
    let err = manager
        .submit_proof(
            BatchRange::of(1, 4),
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
    manager.note_snark_range(BatchRange::of(3, 4)).await;
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
    let manager = ZiskAggregationJobManager::new(ZiskAggregationLaneConfig {
        range_size: K,
        assignment_timeout: Duration::ZERO,
        verification_timeout: Duration::from_secs(60),
        expected_program_vks: HashMap::new(),
        expected_inner_vks: HashMap::new(),
        proof_verification_enabled: true,
        mode: aggregation_mode(MultiProofMode::Required),
    });
    manager.note_snark_range(BatchRange::of(1, 4)).await;
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
    let manager = ZiskAggregationJobManager::new(ZiskAggregationLaneConfig {
        range_size: K,
        assignment_timeout: Duration::from_secs(60),
        verification_timeout: Duration::from_secs(60),
        expected_program_vks: HashMap::from([(TEST_PROTOCOL_VERSION, expected_vk)]),
        expected_inner_vks: HashMap::from([(
            TEST_PROTOCOL_VERSION,
            ZiskVkSet {
                program_vk: B256::repeat_byte(0xA1),
                vadcop_vk: B256::repeat_byte(0xB2),
            },
        )]),
        proof_verification_enabled: true,
        mode: aggregation_mode(MultiProofMode::Required),
    });
    manager.note_snark_range(BatchRange::of(1, 4)).await;
    for batch in 1..=4u64 {
        feed(&manager, batch).await;
    }

    let mut pv = aggregated_pv(B256::ZERO);
    pv[..32].copy_from_slice(expected_vk.as_slice());

    // Not picked yet -> unknown.
    let err = manager
        .submit_proof(
            BatchRange::of(1, 4),
            vec![0; ZISK_SNARK_PROOF_BYTES],
            pv.clone(),
            "agg-1",
        )
        .await
        .expect_err("unassigned range");
    assert!(matches!(
        err,
        ZiskAggregationSubmitError::UnknownRange { .. }
    ));

    manager.pick_next_job("agg-1").await.expect("job");

    // Bad sizes.
    let err = manager
        .submit_proof(BatchRange::of(1, 4), vec![0; 3], pv.clone(), "agg-1")
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
        .submit_proof(
            BatchRange::of(1, 4),
            vec![0; ZISK_SNARK_PROOF_BYTES],
            drifted,
            "agg-1",
        )
        .await
        .expect_err("VK drift");
    assert!(matches!(err, ZiskAggregationSubmitError::VkDrift { .. }));

    // Wrong digest -> rejected, range requeued and re-pickable.
    let err = manager
        .submit_proof(
            BatchRange::of(1, 4),
            vec![0; ZISK_SNARK_PROOF_BYTES],
            pv,
            "agg-1",
        )
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
        .submit_proof(
            BatchRange::of(1, 4),
            vec![0; ZISK_SNARK_PROOF_BYTES],
            pv,
            "agg-2",
        )
        .await
        .expect("accepted");
    assert!(manager.take_completed(BatchRange::of(1, 4)).await.is_some());
}

/// A deterministic binding-digest mismatch is given up on after
/// `MAX_DIGEST_MISMATCH_ATTEMPTS` instead of re-proving the range forever:
/// the range is requeued below the limit and abandoned (no longer pickable
/// or tracked) at it.
#[tokio::test]
async fn persistent_digest_mismatch_gives_up() {
    // Shadow: settlement never waits for this range, so coverage may be shed.
    let manager = manager(MultiProofMode::Shadow);
    manager.note_snark_range(BatchRange::of(1, 4)).await;
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
                BatchRange::of(1, 4),
                vec![0; ZISK_SNARK_PROOF_BYTES],
                wrong.clone(),
                "agg-1",
            )
            .await
            .expect_err("wrong digest rejected");
        assert!(matches!(err, ZiskAggregationSubmitError::DigestMismatch(_)));
        if attempt < MAX_DIGEST_MISMATCH_ATTEMPTS {
            assert_eq!(
                manager.range_status(BatchRange::of(1, 4)).await,
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
        manager.range_status(BatchRange::of(1, 4)).await,
        ZiskAggregationRangeStatus::Unknown,
        "an abandoned range is no longer tracked"
    );
}

/// `Required` inverts that contract. The range composer parks the accepted
/// Airbender proof until this range announces its ZiSK half, and nothing
/// re-registers an abandoned range — so dropping it strands the parked
/// proof and every batch behind it at the commit gate, permanently. The
/// range therefore stays pickable and re-alarms instead.
#[tokio::test]
async fn required_mode_never_abandons_a_range() {
    let manager = manager(MultiProofMode::Required);
    manager.note_snark_range(BatchRange::of(1, 4)).await;
    for batch in 1..=4u64 {
        feed(&manager, batch).await;
    }
    let wrong = aggregated_pv(B256::ZERO);

    // Well past the threshold that Shadow would have given up at.
    for _ in 0..(MAX_DIGEST_MISMATCH_ATTEMPTS * 3) {
        manager
            .pick_next_job("agg-1")
            .await
            .expect("the range stays available for retry");
        let err = manager
            .submit_proof(
                BatchRange::of(1, 4),
                vec![0; ZISK_SNARK_PROOF_BYTES],
                wrong.clone(),
                "agg-1",
            )
            .await
            .expect_err("wrong digest rejected");
        assert!(matches!(err, ZiskAggregationSubmitError::DigestMismatch(_)));
    }
    assert_eq!(
        manager.range_status(BatchRange::of(1, 4)).await,
        ZiskAggregationRangeStatus::InFlight,
        "a range that gates settlement is never abandoned"
    );

    // A repaired prover still lands it.
    let expected = expected_digest(&manager, 1, 4).await;
    manager.pick_next_job("agg-2").await.expect("range");
    manager
        .submit_proof(
            BatchRange::of(1, 4),
            vec![0; ZISK_SNARK_PROOF_BYTES],
            aggregated_pv(expected),
            "agg-2",
        )
        .await
        .expect("the range proves once a working prover picks it up");
}

/// `on_proof_completed` reports the outcome: `Buffered` for a fresh or
/// already-present batch, `BelowFloor` for one whose range was consumed.
/// The per-batch lane keys its completion-marker parking on this.
#[tokio::test]
async fn on_proof_completed_reports_outcome() {
    let manager = manager(MultiProofMode::Required);
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
    manager.note_snark_range(BatchRange::of(5, 5)).await;
    let digest = expected_digest(&manager, 5, 5).await;
    manager.pick_next_job("agg-1").await.expect("range 5..5");
    manager
        .submit_proof(
            BatchRange::of(5, 5),
            vec![0; ZISK_SNARK_PROOF_BYTES],
            aggregated_pv(digest),
            "agg-1",
        )
        .await
        .expect("accepted");
    manager
        .take_completed(BatchRange::of(5, 5))
        .await
        .expect("composed");
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
    let manager = manager(MultiProofMode::Required);
    manager.note_snark_range(BatchRange::of(3, 6)).await;
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
            BatchRange::of(3, 6),
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

    manager.note_snark_range(BatchRange::of(5, 6)).await;
    let job = manager
        .pick_next_job("agg-1")
        .await
        .expect("re-keyed range 5..6");
    assert_eq!((job.from_batch, job.to_batch), (5, 6));

    // A parked completed proof overlapping a later cut is dropped too.
    let digest = expected_digest(&manager, 5, 6).await;
    manager
        .submit_proof(
            BatchRange::of(5, 6),
            vec![0; ZISK_SNARK_PROOF_BYTES],
            aggregated_pv(digest),
            "agg-1",
        )
        .await
        .expect("accepted");
    manager.discard_up_to(6).await;
    assert!(manager.take_completed(BatchRange::of(5, 6)).await.is_none());
}

/// Shadow proving: a range whose batches already settled on L1 keeps its
/// place in the lane. Streams that arrive afterwards still buffer, the
/// range still forms and is still picked, and the proof is still verified —
/// counted as a late verification, with nothing reported lost. Verifying it
/// is also the end of the range: nothing composes, so nothing is parked.
#[tokio::test]
async fn shadow_mode_verifies_a_range_after_settlement() {
    let manager = manager(MultiProofMode::Shadow);
    let lost_before = ZISK_LANE_METRICS.coverage_lost.get();
    let late_before = ZISK_LANE_METRICS.ranges_verified_after_settlement.get();

    // The Airbender lane settles the range before any ZiSK proof arrives.
    manager.note_snark_range(BatchRange::of(1, 4)).await;
    manager.on_batches_settled(4).await;

    for batch in 1..=4u64 {
        assert_eq!(
            manager.on_proof_completed(batch, input(batch as u8)).await,
            AggregationInputOutcome::Buffered,
            "a settled batch must still buffer its stream"
        );
    }
    let job = manager
        .pick_next_job("agg-1")
        .await
        .expect("the settled range is still offered");
    assert_eq!((job.from_batch, job.to_batch), (1, 4));

    let digest = expected_digest(&manager, 1, 4).await;
    manager
        .submit_proof(
            BatchRange::of(1, 4),
            vec![0; ZISK_SNARK_PROOF_BYTES],
            aggregated_pv(digest),
            "agg-1",
        )
        .await
        .expect("the late range proof is verified");

    assert_eq!(
        manager.range_status(BatchRange::of(1, 4)).await,
        ZiskAggregationRangeStatus::Unknown,
        "a verified range is complete in shadow proving"
    );
    assert!(
        manager.take_completed(BatchRange::of(1, 4)).await.is_none(),
        "shadow proving composes nothing, so nothing is parked"
    );
    assert_eq!(manager.queue_counts().await.inputs_buffered, 0);
    assert_eq!(
        ZISK_LANE_METRICS.ranges_verified_after_settlement.get() - late_before,
        1
    );
    assert_eq!(ZISK_LANE_METRICS.coverage_lost.get() - lost_before, 0);
}

/// Feeding the same batch twice keeps the first input (idempotence),
/// and re-noting a known range is a no-op.
#[tokio::test]
async fn duplicate_feed_and_note_are_idempotent() {
    let manager = manager(MultiProofMode::Required);
    manager.note_snark_range(BatchRange::of(1, 1)).await;
    manager.note_snark_range(BatchRange::of(1, 1)).await;
    manager.on_proof_completed(1, input(0xAA)).await;
    manager.on_proof_completed(1, input(0xBB)).await;
    let job = manager.pick_next_job("agg-1").await.expect("range");
    assert_eq!(job.streams[0].1, vec![0xAA; 64]);
    assert!(
        manager.pick_next_job("agg-2").await.is_none(),
        "no duplicate range"
    );
}

/// Verification runs outside the state lock, which is what keeps a verifier's
/// cost from serializing every pick and status read. The range must still hold
/// a lifecycle entry while it runs: the Airbender lane calls `note_snark_range`
/// at both pick and submission, and a range that looked unknown would be
/// re-registered and handed to a second aggregator whose result could then
/// overwrite the first.
#[tokio::test]
async fn a_verifying_range_is_neither_re_registered_nor_re_offered() {
    let manager = manager(MultiProofMode::Required);
    manager.note_snark_range(BatchRange::of(1, 4)).await;
    for batch in 1..=4u64 {
        feed(&manager, batch).await;
    }
    manager
        .pick_next_job("agg-1")
        .await
        .expect("the formed range is offered once");
    manager.mark_verifying_for_test(BatchRange::of(1, 4)).await;

    // The Airbender half arrives while the ZiSK proof is being verified.
    manager.note_snark_range(BatchRange::of(1, 4)).await;

    assert_eq!(
        manager.range_status(BatchRange::of(1, 4)).await,
        ZiskAggregationRangeStatus::InFlight,
        "the range stays known while its proof verifies"
    );
    assert!(
        manager.pick_next_job("agg-2").await.is_none(),
        "a verifying range must not be offered to a second aggregator"
    );
}

/// `Verifying` is recoverable in-process: if the request disappears after the
/// transition, the next Airbender registration returns the range to the queue
/// instead of leaving the parked proof waiting until restart.
#[tokio::test]
async fn an_expired_verifying_range_is_recovered_when_noted() {
    let timeouts_before = ZISK_LANE_METRICS.aggregation_verification_timeouts.get();
    let manager =
        manager_with_verification_timeout(MultiProofMode::Required, Duration::from_millis(10));
    let range = BatchRange::of(1, 4);
    manager.note_snark_range(range).await;
    for batch in range.batches() {
        feed(&manager, batch).await;
    }
    manager.pick_next_job("agg-1").await.expect("first lease");
    manager.mark_verifying_for_test(range).await;

    tokio::time::sleep(Duration::from_millis(50)).await;
    manager.note_snark_range(range).await;

    let state = manager.state.lock().await;
    assert!(
        matches!(state.ranges.get(&range), Some(RangeJob::Pending { .. })),
        "range registration must recover an expired verification attempt"
    );
    drop(state);
    assert!(
        manager.pick_next_job("agg-2").await.is_some(),
        "the recovered range must be offered again"
    );
    assert_eq!(
        ZISK_LANE_METRICS.aggregation_verification_timeouts.get() - timeouts_before,
        1
    );
}

/// Submitted proof bytes are untrusted. A verifier assertion is a rejected
/// attempt, not a panic that unwinds the HTTP task and strands `Verifying`.
#[tokio::test]
async fn blocking_verification_contains_panics() {
    let permit = Arc::new(tokio::sync::Semaphore::new(1))
        .acquire_owned()
        .await
        .expect("verification semaphore stays open");
    let (_, verdict) = run_blocking_verifier(permit, (), |()| -> Result<(), String> {
        panic!("malformed proof")
    })
    .await
    .expect("the blocking task joins");

    assert_eq!(verdict, Err("ZiSK verifier panicked".to_string()));
}

/// A blocking verifier can finish after its timed-out range has been assigned
/// and submitted again. Its result belongs only to the generation that
/// started it and must not overwrite the newer `Verifying` attempt.
#[tokio::test]
async fn a_stale_verification_generation_cannot_finish_a_newer_attempt() {
    let superseded_before = ZISK_LANE_METRICS.superseded_submissions.get();
    let manager =
        manager_with_verification_timeout(MultiProofMode::Required, Duration::from_millis(10));
    let range = BatchRange::of(1, 4);
    manager.note_snark_range(range).await;
    for batch in range.batches() {
        feed(&manager, batch).await;
    }
    manager.pick_next_job("agg-1").await.expect("first lease");
    let stale_generation = manager.mark_verifying_for_test(range).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    manager.note_snark_range(range).await;
    manager.pick_next_job("agg-2").await.expect("second lease");
    let current_generation = manager.mark_verifying_for_test(range).await;
    assert_ne!(stale_generation, current_generation);

    let state = manager.state.lock().await;
    assert!(matches!(
        ZiskAggregationJobManager::verification_failures_for_generation(
            &state,
            range,
            stale_generation,
        ),
        Err(ZiskAggregationSubmitError::Superseded { .. })
    ));
    assert!(matches!(
        state.ranges.get(&range),
        Some(RangeJob::Verifying { generation, .. }) if *generation == current_generation
    ));
    assert_eq!(
        ZISK_LANE_METRICS.superseded_submissions.get() - superseded_before,
        1
    );
}

/// Waiting for native verification capacity is normal load, not a verification
/// attempt. It must leave the external-prover lease assigned, so neither the
/// shorter verification timeout nor request cancellation causes re-proving.
#[tokio::test]
async fn verification_queue_time_does_not_start_the_recovery_timeout() {
    let manager = Arc::new(manager_with_verification_timeout(
        MultiProofMode::Required,
        Duration::from_millis(10),
    ));
    let range = BatchRange::of(1, 4);
    manager.note_snark_range(range).await;
    for batch in range.batches() {
        feed(&manager, batch).await;
    }
    let digest = expected_digest(&manager, range.from(), range.to()).await;
    manager.pick_next_job("agg-1").await.expect("first lease");

    let held_slots = manager
        .verification_slots
        .clone()
        .acquire_many_owned(MAX_CONCURRENT_VERIFICATIONS as u32)
        .await
        .expect("verification semaphore stays open");
    let submitting = tokio::spawn({
        let manager = manager.clone();
        async move {
            manager
                .submit_proof(
                    range,
                    vec![0; ZISK_SNARK_PROOF_BYTES],
                    aggregated_pv(digest),
                    "agg-1",
                )
                .await
        }
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    manager.note_snark_range(range).await;
    assert!(matches!(
        manager.state.lock().await.ranges.get(&range),
        Some(RangeJob::Assigned { .. })
    ));
    assert!(
        manager.pick_next_job("agg-2").await.is_none(),
        "semaphore queue time must not make the range eligible for re-proving"
    );

    submitting.abort();
    assert!(
        submitting
            .await
            .expect_err("task was aborted")
            .is_cancelled()
    );
    drop(held_slots);
    assert!(matches!(
        manager.state.lock().await.ranges.get(&range),
        Some(RangeJob::Assigned { .. })
    ));
}
