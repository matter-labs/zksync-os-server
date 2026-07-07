//! The consensus-storage replay gate.
//!
//! `lib/consensus/sim/fixtures/consensus-storage-v1.bin` is a frozen dump of a real
//! cluster run's consensus-side storage: engine vote journals, marshal's block and
//! finalization archives, its caches and processed-height markers, for every
//! validator. The gate test reopens that storage with the *current* stack and
//! requires the cluster to resume finalizing above the frozen height.
//!
//! What this buys: same-session restarts are exercised all over the sim suite, but
//! they can never catch a *storage-format* regression — both sides of those restarts
//! run the same code. The fixture is the other side of a version boundary. If a
//! commonware upgrade (journal layout, archive encoding, metadata format) or one of
//! our own stack changes stops the node from reading yesterday's disk, this gate
//! fails on the PR that does it, instead of a stage validator failing to boot after
//! a rollout.
//!
//! Regeneration policy: the fixture is regenerated ONLY as a deliberate, reviewed
//! act — when a storage-format migration lands together with its upgrade path. The
//! sequence is: prove the gate still passes on the old fixture under the new code
//! (that is the compatibility claim), then regenerate (`cargo nextest run -p
//! zksync_os_consensus_sim -E 'test(regenerate_consensus_storage_fixture)' --run-ignored all`),
//! commit the new fixture in the same change, and say so in the PR. A PR whose
//! fixture changed without a storage-format story is wrong by definition.

use std::sync::{Arc, Mutex};
use std::time::Duration;
use zksync_os_consensus_sim::fixtures::{
    StorageFixture, candidate_partitions, capture_partitions, restore_partitions,
};
use zksync_os_consensus_sim::{Behavior, EraOptions, MockExecution, SimCluster, fingerprint};

/// The frozen run's geometry. The gate reconstructs validators from the same seed
/// (keys are minted from the runtime's RNG), so these are part of the fixture's
/// identity — the header check below refuses a mismatched fixture.
const SEED: u64 = 42;
const NUM_VALIDATORS: usize = 4;
/// Short epochs so the fixture holds several engine journals and cache generations,
/// not just epoch zero.
const EPOCH_LENGTH: u64 = 8;
/// Freeze after crossing two epoch boundaries.
const TARGET_HEIGHT: u64 = 20;
const STORAGE_PREFIX: &str = "validator";
const VIRTUAL_TIMEOUT: Duration = Duration::from_secs(600);

fn fixture_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join("consensus-storage-v1.bin")
}

fn encode_block(block: &zksync_os_consensus_sim::SimBlock) -> Vec<u8> {
    use commonware_codec::Encode as _;
    block.encode().to_vec()
}

fn decode_block(bytes: &[u8]) -> zksync_os_consensus_sim::SimBlock {
    use commonware_codec::Read as _;
    let mut buf = bytes;
    // Era anchor 0: this scenario runs consensus from its own genesis.
    let block = zksync_os_consensus_sim::SimBlock::read_cfg(&mut buf, &0u64)
        .expect("fixture blocks decode");
    assert!(buf.is_empty(), "trailing bytes in a fixture block");
    block
}

fn tuned_options() -> EraOptions {
    EraOptions {
        stack_tuner: Arc::new(|config| {
            *config = config
                .clone()
                .with_epoch_length(std::num::NonZeroU64::new(EPOCH_LENGTH).expect("nonzero"));
        }),
        ..EraOptions::default()
    }
}

/// The gate: reopen the committed fixture with today's stack and resume the chain.
///
/// Uses [`fingerprint`] directly instead of `run_scenario`: the fixture's keys only
/// exist under [`SEED`], so the nightly `CONSENSUS_SIM_SEEDS` sweep must not apply.
#[test]
fn frozen_consensus_storage_resumes_under_the_current_stack() {
    let bytes = std::fs::read(fixture_path()).unwrap_or_else(|error| {
        panic!(
            "cannot read {} ({error}); the fixture is committed — a missing file means \
             the checkout is broken, not that the gate should be skipped",
            fixture_path().display()
        )
    });
    let fixture = Arc::new(StorageFixture::decode(&bytes));
    assert_eq!(fixture.seed, SEED, "fixture was frozen under another seed");
    assert_eq!(fixture.num_validators as usize, NUM_VALIDATORS);
    assert_eq!(fixture.epoch_length, EPOCH_LENGTH);
    assert!(fixture.height >= TARGET_HEIGHT);

    fingerprint(SEED, VIRTUAL_TIMEOUT, &move |context| {
        let fixture = fixture.clone();
        async move {
            // Restore before any validator starts; the cluster is constructed with
            // every validator provisioned-but-stopped so key minting (the run's
            // first RNG draws, matching the generator) happens over fresh storage,
            // and each start is then an ordinary boot over surviving state.
            restore_partitions(&context, &fixture.partitions).await;
            let behaviors = vec![Behavior::Honest; NUM_VALIDATORS];
            let stopped: Vec<usize> = (0..NUM_VALIDATORS).collect();
            let chains = fixture.chains.clone();
            let mut cluster = SimCluster::start_era(
                context,
                &behaviors,
                zksync_os_consensus_sim::links::healthy(),
                // Each environment gets its frozen chain back: consensus storage
                // and the node's chain survive a restart *together* in production,
                // and marshal's processed-height marker assumes as much.
                move |index, _context| {
                    let blocks = chains[index]
                        .iter()
                        .map(|bytes| decode_block(bytes))
                        .collect();
                    MockExecution::with_committed_chain(blocks)
                },
                EraOptions {
                    stopped,
                    ..tuned_options()
                },
            )
            .await;
            for index in 0..NUM_VALIDATORS {
                cluster.restart(index).await;
            }

            // Resuming *above* the frozen height proves the journals and archives
            // were read, replayed into fresh execution environments, and built on —
            // a stack that silently ignored the restored partitions would restart
            // the chain from scratch and re-finalize height 1, never reach this.
            cluster
                .wait_for_committed_height_all(fixture.height + 5)
                .await;
            cluster.assert_committed_chains_agree(fixture.height);
            cluster.assert_no_faults();
        }
    });
}

/// Regenerates the fixture. `#[ignore]`d: run deliberately, review the diff, commit
/// the new file together with the storage-format change that required it (see the
/// module doc for the full policy).
#[test]
#[ignore = "regenerates the committed fixture; run only for a deliberate storage-format migration"]
fn regenerate_consensus_storage_fixture() {
    let captured: Arc<Mutex<Option<StorageFixture>>> = Arc::new(Mutex::new(None));
    let sink = captured.clone();
    fingerprint(SEED, VIRTUAL_TIMEOUT, &move |context| {
        let sink = sink.clone();
        async move {
            // A capture handle onto the same runtime storage; taken before the
            // cluster consumes the context (contexts are not Clone).
            use commonware_runtime::Supervisor as _;
            let capture_context = context.child("fixture_capture");
            let behaviors = vec![Behavior::Honest; NUM_VALIDATORS];
            let mut cluster = SimCluster::start_era(
                context,
                &behaviors,
                zksync_os_consensus_sim::links::healthy(),
                |_index, _context| MockExecution::new(),
                tuned_options(),
            )
            .await;
            cluster.wait_for_committed_height_all(TARGET_HEIGHT).await;
            // Let in-flight acks and journal writes land before freezing.
            cluster.settle(Duration::from_secs(10)).await;
            let height = cluster.committed_height(0);
            let chains: Vec<Vec<Vec<u8>>> = (0..NUM_VALIDATORS)
                .map(|index| {
                    cluster
                        .env(index)
                        .committed_chain()
                        .into_iter()
                        .map(|block| encode_block(&block))
                        .collect()
                })
                .collect();
            for index in 0..NUM_VALIDATORS {
                cluster.crash(index);
            }

            let max_epoch = height / EPOCH_LENGTH + 3;
            let mut candidates = Vec::new();
            for index in 0..NUM_VALIDATORS {
                candidates.extend(candidate_partitions(
                    &format!("{STORAGE_PREFIX}-{index}"),
                    max_epoch,
                ));
            }
            let partitions = capture_partitions(&capture_context, &candidates).await;
            *sink.lock().expect("capture sink") = Some(StorageFixture {
                seed: SEED,
                num_validators: NUM_VALIDATORS as u32,
                epoch_length: EPOCH_LENGTH,
                height,
                partitions,
                chains,
            });
        }
    });

    let fixture = captured
        .lock()
        .expect("capture sink")
        .take()
        .expect("generator body ran");
    fixture.assert_load_bearing_partitions(STORAGE_PREFIX);
    std::fs::create_dir_all(fixture_path().parent().expect("fixtures dir"))
        .expect("create fixtures dir");
    std::fs::write(fixture_path(), fixture.encode()).expect("write fixture");
    println!(
        "regenerated {} at height {} ({} partitions) — commit it together with the \
         storage-format change that justified regeneration",
        fixture_path().display(),
        fixture.height,
        fixture.partitions.len(),
    );
}
