//! The disaster-fork drill: the full operator choreography on real nodes with
//! real storage — halt, truncate through the tool, deploy the fork
//! configuration, restart, and watch the new era resume and settle. Plus the
//! settler backstop: a truncated settler restarted before the L1 revert step
//! must refuse to start rather than recreate discarded batches.
//!
//! The guard *logic* (acknowledgment matrix, hash cross-check, cutover
//! exactness) is pinned by pure unit tests on `decide_consensus_era`; these
//! tests pin the wiring — the tool against real databases, the era guards on
//! real startups, the batcher against a real L1.

use alloy::eips::BlockId;
use alloy::primitives::{Address, B256};
use alloy::providers::Provider as _;
use std::time::Duration;
use zksync_os_alloy_ext::provider::ZksyncApi as _;
use zksync_os_integration_tests::Tester;
use zksync_os_integration_tests::l1_helpers::wait_for_l1_state;
use zksync_os_integration_tests::multi_node::MultiNodeTester;
use zksync_os_integration_tests::settlement::{CONVERGENCE_TIMEOUT, send_transfer};
use zksync_os_storage_api::ReadBatch as _;

/// The idle heartbeat the drill runs with: long enough that a chain without
/// traffic goes quiet, which is what lets settlement drain to a stable
/// "everything executed" point — the fork anchor must sit at or above it.
const DRILL_IDLE_HEARTBEAT: Duration = Duration::from_secs(300);

/// The tool's manifest, reduced to what the fork config needs.
fn read_manifest(tombstone: &std::path::Path) -> anyhow::Result<(u64, String, Vec<String>)> {
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(tombstone.join("manifest.json"))?)?;
    let truncated_to = manifest["truncated_to"]
        .as_u64()
        .expect("manifest has the truncation point");
    let hash = manifest["hash_at_truncation_point"]
        .as_str()
        .expect("manifest has the anchor hash")
        .to_string();
    let old_hashes = manifest["blocks"]
        .as_array()
        .expect("manifest lists the tombstoned blocks")
        .iter()
        .map(|block| block["hash"].as_str().expect("block hash").to_string())
        .collect();
    Ok((truncated_to, hash, old_hashes))
}

/// The last block of the last batch executed on L1, read the way the tool
/// reads it: L1 names the batch, a node's own batch store maps it to a block.
/// Reads from the first *stopped* node that has the batch persisted; `None`
/// when no listed node does — the persist watcher trails the execute watcher
/// slightly, so a node stopped right at the drain point may not have written
/// the mapping yet.
fn executed_floor_block(cluster: &MultiNodeTester, indices: &[usize], batch: u64) -> Option<u64> {
    if batch == 0 {
        return Some(0);
    }
    for &index in indices {
        let rocks = cluster
            .stopped(index)
            .config()
            .general_config
            .rocks_db_path
            .clone();
        let batches = zksync_os_storage::db::ExecutedBatchStorage::new(&rocks.join("batch"));
        if let Some(persisted) = batches
            .get_batch_by_number(batch)
            .expect("batch store readable")
        {
            return Some(persisted.last_block_number());
        }
    }
    None
}

/// Waits until `node` has the batch→blocks mapping for `batch` on its own
/// disk. Both the floor read above and the truncate tool's executed-floor
/// guard read that mapping from a *stopped* node's store, and the persist
/// watcher trails the execute watcher slightly — pinning the mapping down
/// while the node still runs removes the race against its stop.
async fn wait_for_batch_persisted(node: &Tester, batch: u64) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let persisted: Result<B256, _> = node
            .l2_provider
            .client()
            .request("unstable_getLocalRoot", (batch,))
            .await;
        match persisted {
            Ok(_) => return Ok(()),
            Err(err) => {
                anyhow::ensure!(
                    tokio::time::Instant::now() < deadline,
                    "batch {batch} was not persisted in time: {err}"
                );
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

#[test_log::test(tokio::test)]
async fn the_fork_drill_truncates_reconfigures_and_resumes() -> anyhow::Result<()> {
    // Four validators: the settler stops first per the runbook, and the
    // remaining three (the n=4 quorum) must still finalize the suffix that is
    // about to be declared poisoned — at n=3 the suffix could never be built
    // with the settler already down.
    let mut cluster = MultiNodeTester::start_with_config_overrides(4, |config| {
        config.consensus_config.idle_heartbeat = DRILL_IDLE_HEARTBEAT;
    })
    .await?;

    // A settled prefix: traffic, then let the (now idle) chain drain until
    // everything committed is executed. This point is the fork anchor — the
    // suffix built after it never settles, which is exactly the motivating
    // scenario (an unprovable suffix stays committed-at-most, never executed).
    let recipient = Address::repeat_byte(0x77);
    for _ in 0..2 {
        send_transfer(&cluster, 1, recipient).await?;
    }
    // Fully drained AND stable: everything committed is executed and the
    // executed batch covers the chain tip. The drained point alone is
    // momentary — a sealed-but-unsettled batch can commit right behind it,
    // and the stops below would race it.
    let drained = {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
        loop {
            let state = wait_for_l1_state(cluster.node(0), "settlement fully drained", |state| {
                state.last_executed_batch >= 1
                    && state.last_executed_batch == state.last_committed_batch
            })
            .await?;
            let tip = cluster.node(0).l2_provider.get_block_number().await?;
            let batch_at_tip = cluster
                .node(0)
                .l2_zk_provider
                .get_batch_number_by_block_number(tip)
                .await;
            if batch_at_tip.is_ok_and(|batch| batch == state.last_executed_batch) {
                break state;
            }
            anyhow::ensure!(
                tokio::time::Instant::now() < deadline,
                "settlement did not stabilize at the chain tip in time",
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    };
    // Every node gets truncated below, and both the truncation guard and the
    // fork-point read need the executed batch's block mapping on the node's
    // own disk — pin it down on all of them while they still run.
    for index in 0..4 {
        wait_for_batch_persisted(cluster.node(index), drained.last_executed_batch).await?;
    }

    // The settler goes down first (the runbook's "stop the settler and hold it
    // down"); the surviving quorum keeps producing the suffix that will be
    // declared poisoned.
    cluster.stop_validator(0).await?;
    let mut doomed_tip = 0;
    for _ in 0..2 {
        doomed_tip = send_transfer(&cluster, 1, recipient).await?;
    }
    cluster.stop_validator(1).await?;
    cluster.stop_validator(2).await?;
    cluster.stop_validator(3).await?;
    let fork_point = executed_floor_block(&cluster, &[0, 1, 2, 3], drained.last_executed_batch)
        .expect("every node persisted the executed batch before the stops");
    assert!(
        doomed_tip > fork_point,
        "the doomed suffix must extend past the fork point ({doomed_tip} vs {fork_point})"
    );

    // The tool, on every node. Guards first, fail-first: above the tip and
    // below the L1-executed floor both refuse.
    let node1_config = cluster.stopped(1).config().clone();
    let node1_rocks = node1_config.general_config.rocks_db_path.clone();
    zksync_os_integration_tests::wait_for_rocksdb_locks_released(&node1_rocks).await?;
    let above_tip =
        zksync_os_server::truncate::run_truncate(node1_config.clone(), doomed_tip + 1_000, None)
            .await;
    assert!(
        above_tip.is_err(),
        "truncating above the tip must refuse: {above_tip:?}"
    );
    if fork_point > 0 {
        let below_floor =
            zksync_os_server::truncate::run_truncate(node1_config.clone(), fork_point - 1, None)
                .await;
        assert!(
            below_floor.is_err(),
            "truncating below the L1-executed floor must refuse: {below_floor:?}"
        );
    }

    for index in 0..4 {
        let config = cluster.stopped(index).config().clone();
        let rocks = config.general_config.rocks_db_path.clone();
        zksync_os_integration_tests::wait_for_rocksdb_locks_released(&rocks).await?;
        zksync_os_server::truncate::run_truncate(config, fork_point, None).await?;
    }

    // The settler ended exactly at the fork point (its chain never saw the
    // suffix), so its truncation was a no-op and wrote no tombstone; a node
    // that carried the suffix exported it. The manifest names the anchor for
    // the fork configuration.
    let (truncated_to, anchor_hash, old_hashes) =
        read_manifest(&node1_rocks.join(format!("tombstone-{fork_point}")))?;
    assert_eq!(truncated_to, fork_point);
    assert!(
        !old_hashes.is_empty(),
        "the tombstone names the discarded blocks"
    );

    // Restarting over pre-truncation consensus engine state must refuse by
    // name: the tool flags the state it invalidated, and running consensus
    // over it would die mid-run on the delivery-order assert instead.
    let refusal_hash = anchor_hash.clone();
    let refused = cluster
        .start_validator_with_config_overrides(1, move |config| {
            config.consensus_config.genesis_height = fork_point;
            config.consensus_config.protocol_version = 2;
            config.consensus_config.acknowledge_fork = Some(format!("{fork_point}:{refusal_hash}"));
        })
        .await;
    let refusal = refused.expect_err("pre-truncation consensus state must refuse to start");
    assert!(
        refusal.to_string().contains("truncated"),
        "the refusal names the truncation: {refusal:#}"
    );

    // The fork start requires cleared consensus engine state — the documented
    // operator step; the tool deliberately leaves consensus storage alone
    // (flag included).
    for index in 0..4 {
        let consensus_dir = cluster
            .stopped(index)
            .config()
            .general_config
            .rocks_db_path
            .join("consensus");
        if consensus_dir.exists() {
            std::fs::remove_dir_all(&consensus_dir)?;
        }
    }

    // The fork configuration: new anchor, bumped protocol version, the
    // acknowledgment naming exactly what is being started into. Settler last
    // per the runbook.
    for index in [1, 2, 3, 0] {
        let anchor_hash = anchor_hash.clone();
        cluster
            .start_validator_with_config_overrides(index, move |config| {
                config.consensus_config.genesis_height = fork_point;
                config.consensus_config.protocol_version = 2;
                config.consensus_config.acknowledge_fork =
                    Some(format!("{fork_point}:{anchor_hash}"));
            })
            .await?;
    }

    // The new era lives: blocks land above the fork point, every node agrees on
    // them, and the block that replaced the first discarded one is a different
    // block (the suffix was re-decided, not restored).
    let mut included_at = 0;
    for _ in 0..2 {
        included_at = send_transfer(&cluster, 1, recipient).await?;
    }
    assert!(included_at > fork_point);
    cluster
        .wait_for_block_on_all(included_at, CONVERGENCE_TIMEOUT)
        .await?;
    cluster.assert_block_hashes_agree(included_at).await?;
    let new_first = cluster
        .node(1)
        .l2_provider
        .get_block(BlockId::number(fork_point + 1))
        .await?
        .expect("the new era's first block exists");
    assert_ne!(
        format!("{:?}", new_first.header.hash),
        old_hashes[0],
        "the new era's first block must differ from the tombstoned one"
    );

    // Settlement resumes over the re-decided suffix: new batches commit and
    // execute past the pre-fork drain point.
    wait_for_l1_state(cluster.node(0), "the new era settles", |state| {
        state.last_executed_batch > drained.last_executed_batch
    })
    .await?;
    Ok(())
}

#[test_log::test(tokio::test)]
async fn a_truncated_settler_refuses_to_restart_while_l1_is_ahead() -> anyhow::Result<()> {
    let mut cluster = MultiNodeTester::start(3).await?;
    let recipient = Address::repeat_byte(0x78);

    // Catch L1 with batches committed but not yet executed, and hold the
    // settler down at that moment — the state a fork's L1-revert step exists
    // for. The window between a commit landing and its execution is real but
    // narrow; traffic re-arms it until the stop wins the race.
    let mut caught = None;
    for _ in 0..10 {
        send_transfer(&cluster, 1, recipient).await?;
        let state = wait_for_l1_state(cluster.node(0), "a commit landed", |state| {
            state.last_committed_batch >= 1
        })
        .await?;
        if state.last_committed_batch > state.last_executed_batch {
            cluster.stop_validator(0).await?;
            let rocks = cluster
                .stopped(0)
                .config()
                .general_config
                .rocks_db_path
                .clone();
            zksync_os_integration_tests::wait_for_rocksdb_locks_released(&rocks).await?;
            // Re-check after the stop: execution may have landed mid-stop. The
            // truncation below also reads the executed floor from the stopped
            // settler's own disk, so a stop that outran the persist watcher
            // re-arms like a missed window.
            let after =
                wait_for_l1_state(cluster.node(1), "settle-state re-read", |_| true).await?;
            if after.last_committed_batch > after.last_executed_batch
                && let Some(floor) = executed_floor_block(&cluster, &[0], after.last_executed_batch)
            {
                caught = Some((after, floor));
                break;
            }
            cluster.start_validator(0).await?;
        }
    }
    let Some((caught, floor)) = caught else {
        // The commit→execute window never survived a stop on this machine —
        // the guard's logic is still pinned by its message assertion below
        // being unreachable; bail out loudly rather than pretend.
        anyhow::bail!("could not catch L1 with committed > executed; rerun");
    };

    // Truncate the settler to the executed floor: legal per the guards, but it
    // leaves committed-but-unexecuted batches above the local chain on L1.
    let config = cluster.stopped(0).config().clone();
    zksync_os_server::truncate::run_truncate(config, floor, None).await?;

    // Restarting it without reverting those batches must refuse, naming the
    // revert step — the recovery machinery would otherwise faithfully
    // recreate and re-commit exactly the discarded blocks.
    let refused = cluster.start_validator(0).await;
    assert!(
        refused.is_err(),
        "a settler behind L1's committed range must refuse to start"
    );

    // The runbook's revert step needs no new tooling: the standalone
    // `L1Revert` rebuild mode reverts the committed-but-unexecuted batches at
    // startup, after which the very same restart passes the guard and the
    // settler resumes from the truncated chain. Like every truncated node
    // (the runbook's "clear the consensus engine state" step), it restarts
    // with cleared consensus storage — the engine's journals and archives
    // still reference the heights above the truncated write-ahead log.
    let consensus_dir = cluster
        .stopped(0)
        .config()
        .general_config
        .rocks_db_path
        .join("consensus");
    if consensus_dir.exists() {
        std::fs::remove_dir_all(&consensus_dir)?;
    }
    let first_reverted = caught.last_executed_batch + 1;
    let commit_tx_hash =
        crate::node::rebuild::fetch_on_chain_batch_commit_tx_hash(cluster.node(1), first_reverted)
            .await?;
    let reverter = crate::node::rebuild::make_reverter_config(cluster.stopped(0))?;
    cluster
        .start_validator_with_config_overrides(0, move |config| {
            config.sequencer_config.rebuild =
                Some(zksync_os_server::config::RebuildConfig::L1Revert {
                    from_batch_number: std::num::NonZeroU64::new(first_reverted)
                        .expect("executed + 1 is nonzero"),
                    from_batch_commit_tx_hash: commit_tx_hash,
                    l1_reverter_sk: reverter.clone(),
                });
        })
        .await?;
    Ok(())
}
