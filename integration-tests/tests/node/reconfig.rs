//! Live committee reconfiguration over real nodes: the validator set changes at an
//! epoch boundary while the chain keeps producing, verifying, and settling real
//! blocks. The deterministic simulation pins the same choreographies at the
//! protocol level (`lib/consensus/sim/tests/reconfig.rs`); these tests add what
//! only real processes can prove — real p2p connections and bans, process
//! restarts as the configuration-fix mechanism, `/status` surfaces, and the
//! custody records in the node's own finality store.

use std::time::Duration;
use zksync_os_integration_tests::multi_node::MultiNodeTester;
use zksync_os_server::config::CommitteeScheduleEntryConfig;

const CONVERGENCE_TIMEOUT: Duration = Duration::from_secs(120);
/// The promotion test's boundary wait spans the rolling restarts plus a dozen
/// epochs of margin — generous on purpose.
const PROMOTION_TIMEOUT: Duration = Duration::from_secs(240);

/// Blocks per epoch for these tests: small enough to cross several boundaries in
/// seconds, large enough that a boundary is not every other block.
const EPOCH_LENGTH: u64 = 20;

/// The committee grows 3 → 4 at epoch 2, live. The joiner runs from genesis,
/// follows the epochs it is not yet a member of, then votes from its activation
/// boundary — proven by quorum arithmetic (stop an original member; the remaining
/// three of four can only advance if the joiner signs). Also pins the two
/// observability surfaces of a committee change: `/status` reporting the
/// finalized epoch's committee size, and the custody records in the finality
/// store naming each epoch's committee.
#[test_log::test(tokio::test)]
async fn committee_grows_at_an_epoch_boundary_live() -> anyhow::Result<()> {
    let schedule = vec![(0, vec![0, 1, 2]), (2, vec![0, 1, 2, 3])];
    let mut cluster = MultiNodeTester::start_with_schedule(4, &schedule, EPOCH_LENGTH).await?;

    // Cross the activation boundary with everyone healthy; all four nodes —
    // including the joiner, which followed epochs 0–1 without voting — converge.
    cluster
        .wait_for_block_on_all(2 * EPOCH_LENGTH + 5, CONVERGENCE_TIMEOUT)
        .await?;
    cluster
        .assert_block_hashes_agree(2 * EPOCH_LENGTH + 5)
        .await?;

    // `/status` on the joiner reflects the change: the finalized round is in
    // epoch 2+, held by the grown committee.
    let status = cluster.node(3).status().await?;
    let consensus = status.consensus.expect("validators report consensus");
    let finalized = consensus.finalized.expect("finalizing");
    anyhow::ensure!(finalized.epoch >= 2, "expected epoch 2+, got {finalized:?}");
    anyhow::ensure!(
        finalized.committee_size == 4,
        "the finalized epoch's committee must be the grown one, got {finalized:?}"
    );

    // The joiner votes: with one original member stopped, the epoch-2 committee
    // of 4 (quorum 3) advances only if the joiner signs.
    cluster.stop_validator(1).await?;
    let target = cluster.max_height().await? + EPOCH_LENGTH;
    cluster
        .wait_for_block_on_all(target, CONVERGENCE_TIMEOUT)
        .await?;
    cluster.assert_block_hashes_agree(target).await?;

    // The custody trail: the batcher node's finality store names a committee per
    // observed epoch — 3 members before the change, 4 after. Stop just that node
    // (a stopped validator keeps its state on disk; a full shutdown would delete
    // the test directories) and read its store directly.
    let rocks = cluster
        .node(0)
        .config()
        .general_config
        .rocks_db_path
        .clone();
    cluster.stop_validator(0).await?;
    zksync_os_integration_tests::wait_for_rocksdb_locks_released(&rocks).await?;
    let store = zksync_os_consensus_execution::FinalityStore::open(&rocks.join("finality"))?;
    let epoch0 = store
        .epoch_transition(0)?
        .expect("epoch 0 custody record exists");
    anyhow::ensure!(
        epoch0.committee.len() == 3,
        "epoch 0 was held by the original committee of 3"
    );
    let epoch2 = store
        .epoch_transition(2)?
        .expect("epoch 2 custody record exists");
    anyhow::ensure!(
        epoch2.committee.len() == 4,
        "epoch 2 is held by the grown committee of 4"
    );
    drop(store);
    cluster.shutdown_all().await?;
    Ok(())
}

/// The committee shrinks 4 → 3 at epoch 2, live. The excluded validator keeps
/// running: it stops voting (it builds no engine for epochs it is not scheduled
/// into) but — because it remains in every member's address book — it keeps
/// *following* the chain as an observer, verifying finalizations via the
/// certificate backup lane and backfilling blocks. Its RPC keeps serving the
/// growing chain, which is exactly the state an operator wants a machine in
/// while repointing it as an external node.
#[test_log::test(tokio::test)]
async fn committee_shrinks_at_an_epoch_boundary_live() -> anyhow::Result<()> {
    let schedule = vec![(0, vec![0, 1, 2, 3]), (2, vec![0, 1, 2])];
    let cluster = MultiNodeTester::start_with_schedule(4, &schedule, EPOCH_LENGTH).await?;

    // Cross the boundary and keep growing on the smaller committee (3 of 3 —
    // every remaining vote is needed, so progress itself proves the excluded
    // validator's absence from the committee does not stall anything).
    cluster
        .wait_for_block_on_all(3 * EPOCH_LENGTH, CONVERGENCE_TIMEOUT)
        .await?;
    // ...and the excluded validator followed the whole way (wait_for_block_on_all
    // above already required its RPC to reach the height); the chains agree.
    cluster.assert_block_hashes_agree(3 * EPOCH_LENGTH).await?;

    // Its status tells the story: the finalized epoch is 2+ under a committee of
    // 3 — a committee this validator can verify but is not part of.
    let status = cluster.node(3).status().await?;
    let consensus = status.consensus.expect("consensus section");
    let finalized = consensus.finalized.expect("still observing finality");
    anyhow::ensure!(finalized.epoch >= 2, "observer fell behind: {finalized:?}");
    anyhow::ensure!(
        finalized.committee_size == 3,
        "the finalized epoch's committee must be the shrunk one, got {finalized:?}"
    );
    cluster.shutdown_all().await?;
    Ok(())
}

/// The operator-error recovery the simulation cannot model (its network has no
/// ban-clearing): a validator whose config is missing the newest committee entry
/// crosses the activation boundary on the old committee, cannot verify the real
/// committee's certificates, and falls behind — safely, disrupting nobody.
///
/// The remedy is a *rebuild*, not an in-place restart: consensus vote journals
/// encode signers by committee position, so votes journaled under the wrong
/// committee decode as another signer's under the corrected one — the engine
/// refuses the replay, loudly (`replaying nullify from another signer`), which is
/// the correct reaction to structurally unsound state. Discovered here and
/// registered: an in-place config fix is only sound for a validator that never
/// crossed the boundary misconfigured. The rebuild relaunches on the corrected
/// schedule with a fresh data directory: the node re-bootstraps from L1, then
/// backfills and re-verifies the whole chain from its peers — the same path a
/// brand-new validator takes — and votes again.
#[test_log::test(tokio::test)]
async fn misconfigured_validator_stalls_then_recovers_after_config_fix() -> anyhow::Result<()> {
    let corrected = vec![(0, vec![0, 1, 2]), (2, vec![0, 1, 2, 3])];
    let stale = vec![(0, vec![0, 1, 2])];
    let mut cluster = MultiNodeTester::start_with_schedule_and_overrides(
        4,
        &corrected,
        EPOCH_LENGTH,
        &[(1, stale)],
    )
    .await?;

    // The correctly-configured members (3 of the new committee of 4 — a quorum)
    // carry the chain across the boundary. The misconfigured validator stalls
    // near it: `wait_for_block_on_all` cannot be used while it lags, so wait on
    // the healthy members individually.
    let target = 3 * EPOCH_LENGTH;
    for index in [0, 2, 3] {
        wait_for_height_on(&cluster, index, target).await?;
    }
    // The misconfigured validator lags materially rather than freezing outright:
    // with the old committee being a prefix of the new one, its stale engine can
    // still assemble (stale-width) certificates from the members' raw votes for
    // members-led blocks — but never for joiner-led ones, and it cannot verify
    // the real committee's certificates at all. Materially behind and falling
    // further back is the honest observable here.
    let lagging = cluster
        .node(1)
        .status()
        .await?
        .consensus
        .expect("consensus section")
        .applied_height
        .unwrap_or(0);
    anyhow::ensure!(
        lagging + EPOCH_LENGTH / 2 < target,
        "a validator without the new committee entry must fall behind the real \
         committee (applied {lagging}, members at {target}+)"
    );

    // The remedy, exactly as the runbook documents it: corrected config + a
    // fresh CONSENSUS data directory — the chain, the write-ahead log, and the
    // finality store all stay. The restart resumes from a cached finality floor
    // (observed in this scenario: the last finalization before the committee
    // change, one height below the boundary) instead of replaying consensus
    // history from genesis; catch-up is bounded to the blocks above it. The
    // floor-engagement mechanics are DST-pinned in the sim's promotion tests;
    // this proves the wiring over a real node's finality store.
    //
    // `accept_stale_floor` rides along because where the stall lands varies:
    // a validator that observed even one finalization of the epoch it stalled
    // in has a custody record for it, and then every USABLE floor (from before
    // the change) fails the freshness policy — the flag is the runbook's escape
    // hatch for exactly this rebuild, harmless in the runs where the floor is
    // fresh anyway.
    let stalled_rocks = cluster
        .node(1)
        .config()
        .general_config
        .rocks_db_path
        .clone();
    cluster.stop_validator(1).await?;
    zksync_os_integration_tests::wait_for_rocksdb_locks_released(&stalled_rocks).await?;
    std::fs::remove_dir_all(stalled_rocks.join("consensus"))?;
    let corrected_entries: Vec<CommitteeScheduleEntryConfig> = corrected
        .iter()
        .map(|(activation_epoch, indices)| CommitteeScheduleEntryConfig {
            activation_epoch: *activation_epoch,
            validators: indices
                .iter()
                .map(|&i| cluster.committee_entry(i).to_string())
                .collect(),
        })
        .collect();
    cluster
        .start_validator_with_config_overrides(1, move |config| {
            config.consensus_config.committees = corrected_entries;
            config.consensus_config.accept_stale_floor = true;
        })
        .await?;

    // Catch-up, then participation: with one other member stopped, quorum needs
    // the recovered validator's votes. (The tip is read before the restart —
    // the recovering node's RPC takes a moment to come back.)
    wait_for_height_on(&cluster, 1, target).await?;
    cluster.stop_validator(0).await?;
    let target = cluster.max_height().await? + EPOCH_LENGTH;
    for index in [1, 2, 3] {
        wait_for_height_on(&cluster, index, target).await?;
    }
    cluster.assert_block_hashes_agree(target).await?;
    cluster.shutdown_all().await?;
    Ok(())
}

/// Waits until one validator's RPC serves at least `height`.
async fn wait_for_height_on(
    cluster: &MultiNodeTester,
    index: usize,
    height: u64,
) -> anyhow::Result<()> {
    use alloy::providers::Provider as _;
    let deadline = tokio::time::Instant::now() + CONVERGENCE_TIMEOUT;
    loop {
        let number = cluster
            .node(index)
            .l2_provider
            .get_block_number()
            .await
            .unwrap_or(0);
        if number >= height {
            return Ok(());
        }
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "validator {index} did not reach block {height} within {CONVERGENCE_TIMEOUT:?} \
             (at {number})",
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// The full promotion choreography, live: a node that started life as a
/// non-voting OBSERVER (no BLS key configured, admitted via the observers list)
/// is scheduled into the committee at a future epoch and becomes a voting
/// validator — without ever resyncing. The steps mirror the runbook exactly:
///
/// 1. every sitting validator restarts with the appended schedule entry (and the
///    candidate moved out of the admission list — it is committee-listed now,
///    which also keeps it connectable throughout: the address book spans every
///    schedule entry);
/// 2. the candidate restarts last, flipping `role = validator` and gaining its
///    BLS key — over its RETAINED chain and consensus archives from observing;
/// 3. nothing special happens at the boundary itself: the rotation starts the
///    candidate's first-ever engine because the schedule now says "member".
#[test_log::test(tokio::test)]
async fn observer_promoted_to_validator_at_a_scheduled_boundary() -> anyhow::Result<()> {
    // 4 validators (quorum 3 — each rolling restart leaves the chain live) and
    // one observer, the candidate, at index 4.
    let schedule = vec![(0, vec![0, 1, 2, 3])];
    let mut cluster =
        MultiNodeTester::start_with_schedule_and_observers(4, 1, &schedule, EPOCH_LENGTH).await?;
    const CANDIDATE: usize = 4;

    // The candidate observes: reaching a height proves it applies
    // consensus-delivered blocks (it has no other source).
    cluster
        .wait_for_block_on_all(EPOCH_LENGTH / 2, CONVERGENCE_TIMEOUT)
        .await?;

    // The promotion target: far enough out for five sequential restarts. All
    // configs must name the SAME activation epoch, so pick it before touching
    // anyone. (If the restarts overran it, the committee would simply run one
    // member short until the candidate arrives — late first engines are safe —
    // but the margin keeps the test deterministic in what it asserts.)
    let activation_epoch = cluster.max_height().await? / EPOCH_LENGTH + 12;
    let promoted_entries: Vec<CommitteeScheduleEntryConfig> = vec![
        CommitteeScheduleEntryConfig {
            activation_epoch: 0,
            validators: (0..4)
                .map(|i| cluster.committee_entry(i).to_string())
                .collect(),
        },
        CommitteeScheduleEntryConfig {
            activation_epoch,
            validators: (0..5)
                .map(|i| cluster.committee_entry(i).to_string())
                .collect(),
        },
    ];

    // Step 1: rolling schedule deployment across the sitting committee.
    for index in 0..4 {
        cluster.stop_validator(index).await?;
        let entries = promoted_entries.clone();
        cluster
            .start_validator_with_config_overrides(index, move |config| {
                config.consensus_config.committees = entries;
                // The candidate is committee-listed now; a key may not be both.
                config.consensus_config.observers = vec![];
            })
            .await?;
    }

    // Step 2: the candidate flips roles — same chain, same consensus archives,
    // plus a signing key and the schedule that makes it a member at E.
    let bls_key = cluster.bls_key_hex(CANDIDATE).to_string();
    cluster.stop_validator(CANDIDATE).await?;
    let entries = promoted_entries.clone();
    cluster
        .start_validator_with_config_overrides(CANDIDATE, move |config| {
            config.consensus_config.role = zksync_os_server::config::ConsensusRole::Validator;
            config.consensus_config.bls_key = Some(bls_key);
            config.consensus_config.committees = entries;
            config.consensus_config.observers = vec![];
            config.consensus_config.tx_forward_rpc_urls = vec![];
        })
        .await?;

    // Step 3: cross the activation boundary and converge past it — everyone,
    // the promoted member included.
    let past_boundary = activation_epoch * EPOCH_LENGTH + 5;
    cluster
        .wait_for_block_on_all(past_boundary, PROMOTION_TIMEOUT)
        .await?;
    cluster.assert_block_hashes_agree(past_boundary).await?;

    // The promoted node's own view: a validator now, finalizing in the grown
    // committee's epoch.
    let status = cluster.node(CANDIDATE).status().await?;
    let consensus = status.consensus.expect("consensus section");
    anyhow::ensure!(consensus.role == "validator", "role flip must be visible");
    let finalized = consensus.finalized.expect("finalizing");
    anyhow::ensure!(
        finalized.epoch >= activation_epoch && finalized.committee_size == 5,
        "expected the promoted committee of 5 at epoch {activation_epoch}+, got {finalized:?}"
    );

    // And it VOTES: with one original member stopped, the committee of 5
    // (quorum 4) advances only if the promoted member signs.
    cluster.stop_validator(1).await?;
    let target = cluster.max_height().await? + EPOCH_LENGTH;
    cluster
        .wait_for_block_on_all(target, CONVERGENCE_TIMEOUT)
        .await?;
    cluster.assert_block_hashes_agree(target).await?;

    // The custody trail records the handoff: the committee of 4 before the
    // activation epoch, 5 from it.
    let rocks = cluster
        .node(0)
        .config()
        .general_config
        .rocks_db_path
        .clone();
    cluster.stop_validator(0).await?;
    zksync_os_integration_tests::wait_for_rocksdb_locks_released(&rocks).await?;
    let store = zksync_os_consensus_execution::FinalityStore::open(&rocks.join("finality"))?;
    let promoted = store
        .epoch_transition(activation_epoch)?
        .expect("activation epoch custody record exists");
    anyhow::ensure!(
        promoted.committee.len() == 5,
        "the activation epoch is held by the promoted committee of 5"
    );
    drop(store);
    cluster.shutdown_all().await?;
    Ok(())
}
