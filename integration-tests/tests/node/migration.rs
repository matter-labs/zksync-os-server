//! Migration-at-height over real nodes: a single-sequencer chain is drained, its
//! state is distributed to a fresh committee (snapshot copy — the v1 migration
//! path), and consensus continues the chain from the agreed anchor. And back:
//! rollback restarts a single sequencer from the same write-ahead log, which is
//! valid at any point because it only ever contained finalized blocks.
//!
//! The guard matrix itself (first start must be exactly at the anchor, era
//! mismatches over existing consensus state refuse to start, single-sequencer
//! refuses over consensus state without acknowledgment) is pinned by unit tests
//! next to the guards; these tests cover the operational flows.

use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use std::time::Duration;
use zksync_os_integration_tests::multi_node::{CommitteeSeat, MultiNodeTester};
use zksync_os_integration_tests::{CURRENT_TO_L1, Tester, test_multisetup};

// Sized for a loaded machine (a full-suite run packs several committees
// concurrently, and CI runners are slower still), not for the idle happy path.
const CONVERGENCE_TIMEOUT: Duration = Duration::from_secs(120);

/// Submits a transfer through the given node and returns (block number, tx hash).
///
/// The receipt is polled directly rather than through the provider's
/// block-subscription watcher (see `settlement::send_transfer` for why); on a
/// timeout the error says whether the node still holds the transaction.
async fn transfer_via(
    node: &Tester,
    recipient: Address,
    value: U256,
) -> anyhow::Result<(u64, alloy::primitives::B256)> {
    let submitted = node
        .l2_provider
        .send_transaction(
            TransactionRequest::default()
                .with_to(recipient)
                .with_value(value),
        )
        .await?;
    let hash = *submitted.tx_hash();

    let deadline = tokio::time::Instant::now() + CONVERGENCE_TIMEOUT;
    loop {
        if let Some(receipt) = node.l2_provider.get_transaction_receipt(hash).await? {
            anyhow::ensure!(receipt.status(), "transfer {hash} reverted");
            return Ok((
                receipt.block_number.expect("included txs have a block"),
                receipt.transaction_hash,
            ));
        }
        if tokio::time::Instant::now() >= deadline {
            let still_known = node
                .l2_provider
                .get_transaction_by_hash(hash)
                .await?
                .is_some();
            anyhow::bail!(
                "transfer {hash} was not included within {CONVERGENCE_TIMEOUT:?}; \
                 the node {}",
                if still_known {
                    "still holds it unincluded"
                } else {
                    "no longer knows it"
                },
            );
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[test_multisetup([CURRENT_TO_L1])]
async fn single_sequencer_chain_migrates_to_a_committee(main_node: Tester) -> anyhow::Result<()> {
    // The pre-consensus era: ordinary single-sequencer operation with real traffic
    // whose effects must survive the cutover.
    let recipient = Address::repeat_byte(0x61);
    let value = U256::from(7_000_000u64);
    let (pre_migration_block, pre_migration_tx) =
        transfer_via(&main_node, recipient, value).await?;

    // Drain: stop the sequencer. Its write-ahead-log tip becomes the anchor; the
    // harness reads it from the stopped node's database exactly like a migration
    // operator would.
    let stopped = main_node.stop().await?;

    // Cutover: three validators start on copies of the drained chain.
    let cluster = MultiNodeTester::migrate_from(&stopped, 3).await?;
    let anchor = cluster.max_height().await?;
    assert!(
        anchor >= pre_migration_block,
        "the anchor must include pre-migration history",
    );

    // The committee continues the chain past the anchor, in agreement.
    cluster
        .wait_for_block_on_all(anchor + 3, CONVERGENCE_TIMEOUT)
        .await?;
    cluster.assert_block_hashes_agree(anchor + 3).await?;

    // The pre-consensus era is ordinary history on every validator: balances and
    // receipts from before the migration are served as if nothing happened.
    for index in 0..cluster.len() {
        let balance = cluster
            .node(index)
            .l2_provider
            .get_balance(recipient)
            .await?;
        assert_eq!(
            balance, value,
            "validator {index} does not serve pre-migration state",
        );
        assert!(
            cluster
                .node(index)
                .l2_provider
                .get_transaction_receipt(pre_migration_tx)
                .await?
                .is_some(),
            "validator {index} does not serve pre-migration receipts",
        );
    }

    // New traffic flows through the committee — submitted to a NON-batcher
    // validator, finalized by consensus, visible everywhere.
    let (included_at, _) = transfer_via(cluster.node(1), recipient, value).await?;
    assert!(
        included_at > anchor,
        "new traffic lands in the consensus era"
    );
    cluster
        .wait_for_block_on_all(included_at, CONVERGENCE_TIMEOUT)
        .await?;
    cluster.assert_block_hashes_agree(included_at).await?;

    // The finality-certificate trail starts at the anchor (pre-consensus history
    // needs no certificates — the era floor covers it) and must cover the new block.
    let deadline = tokio::time::Instant::now() + CONVERGENCE_TIMEOUT;
    loop {
        let certified = cluster
            .node(1)
            .status()
            .await?
            .consensus
            .and_then(|consensus| consensus.finality_certified_height);
        if certified.unwrap_or(0) >= included_at {
            break;
        }
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "certificates did not cover block {included_at} (certified: {certified:?})",
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    cluster.shutdown_all().await
}

#[test_multisetup([CURRENT_TO_L1])]
async fn a_scheduled_cutover_migrates_a_running_chain(main_node: Tester) -> anyhow::Result<()> {
    // Pre-cutover traffic on the plain single sequencer.
    let recipient = Address::repeat_byte(0x64);
    let value = U256::from(5_000_000u64);
    let (pre_block, _) = transfer_via(&main_node, recipient, value).await?;

    // The anchor is agreed ahead of time, comfortably above the current tip —
    // nobody reads it from a drained database in this flow.
    let tip = main_node.l2_provider.get_block_number().await?;
    let anchor = tip + 4;

    // Two committee seats: the sequencer and the external node that follows it.
    let seat_main = CommitteeSeat::reserve().await?;
    let seat_en = CommitteeSeat::reserve().await?;
    let committee = vec![seat_main.committee_entry(), seat_en.committee_entry()];

    // One config deploy per node: the same file serves the pre-cutover role and
    // the consensus node after it. The sequencer keeps `node_role = main`...
    let main_node = main_node
        .stop()
        .await?
        .start_with_overrides(|config| {
            seat_main.arm_consensus(config, committee.clone(), anchor);
        })
        .await?;
    // ...and the follower keeps `node_role = external` (its pre-cutover
    // behavior); the batcher stays on exactly one node. The follower's config
    // must already carry every main-node fact it needs from the anchor on —
    // the harness's external-node defaults strip the pubdata mode, so restore
    // the chain's value.
    let pubdata_mode = main_node.config().l1_sender_config.pubdata_mode;
    let en = main_node
        .launch_external_node_overrides(|config| {
            seat_en.arm_consensus(config, committee.clone(), anchor);
            config.batcher_config.enabled = false;
            config.l1_sender_config.pubdata_mode = pubdata_mode;
        })
        .await?;

    let mut main_cutover = main_node
        .scheduled_cutover_reached
        .clone()
        .expect("the sequencer armed a scheduled cutover");
    let mut en_cutover = en
        .scheduled_cutover_reached
        .clone()
        .expect("the follower armed a scheduled cutover");

    // Drive traffic across the anchor. Submissions past it stay pending by
    // design (the sequencer seals nothing above the anchor), so nothing here
    // waits for receipts.
    let driver = async {
        loop {
            let _ = main_node
                .l2_provider
                .send_transaction(
                    TransactionRequest::default()
                        .with_to(recipient)
                        .with_value(U256::from(1u64)),
                )
                .await;
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    };
    let cutovers = async {
        main_cutover.wait_for(|reached| *reached).await?;
        en_cutover.wait_for(|reached| *reached).await?;
        anyhow::Ok(())
    };
    tokio::select! {
        result = tokio::time::timeout(CONVERGENCE_TIMEOUT, cutovers) => result??,
        _ = driver => unreachable!("the traffic driver never finishes"),
    }

    // Both nodes stopped producing/following at exactly the anchor, and the
    // pending cutover is visible on the status surface.
    assert_eq!(main_node.l2_provider.get_block_number().await?, anchor);
    assert_eq!(en.l2_provider.get_block_number().await?, anchor);
    let pending = main_node
        .status()
        .await?
        .scheduled_cutover
        .expect("a pending cutover surfaces in /status");
    assert_eq!(pending.genesis_height, anchor);
    assert_eq!(pending.tip, anchor);

    // The supervisor's restart, played by the test: stop both, start both with
    // the exact same configuration. The next boot finds the log ending at the
    // anchor and runs consensus.
    let en_stopped = en.stop().await?;
    let main_stopped = main_node.stop().await?;
    let main_node = main_stopped.start().await?;
    let en = en_stopped.start().await?;

    // The committee (both nodes — n=2 needs both) continues the chain past the
    // anchor; the ex-follower is a validator now and includes traffic.
    let (post_block, _) = transfer_via(&en, recipient, value).await?;
    assert!(
        post_block > anchor,
        "consensus-era traffic lands above the anchor"
    );
    assert!(
        pre_block < anchor,
        "sanity: pre-cutover history sits below the anchor"
    );

    // Agreement at the consensus era's first blocks, and the status surfaces
    // flipped from pending-cutover to consensus on both nodes.
    let deadline = tokio::time::Instant::now() + CONVERGENCE_TIMEOUT;
    loop {
        let (main_block, en_block) = (
            main_node
                .l2_provider
                .get_block(alloy::eips::BlockId::number(post_block))
                .await?
                .map(|block| block.header.hash),
            en.l2_provider
                .get_block(alloy::eips::BlockId::number(post_block))
                .await?
                .map(|block| block.header.hash),
        );
        if let (Some(main_hash), Some(en_hash)) = (main_block, en_block) {
            assert_eq!(
                main_hash, en_hash,
                "the committee disagrees at {post_block}"
            );
            break;
        }
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "block {post_block} did not reach both nodes in time",
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    for node in [&main_node, &en] {
        let status = node.status().await?;
        assert!(
            status.scheduled_cutover.is_none(),
            "no cutover is pending once consensus runs"
        );
        assert!(
            status.consensus.is_some(),
            "the consensus section is live after the cutover"
        );
    }

    // Pre-cutover history is ordinary chain history to the committee: both big
    // transfers serve (the driver's 1-wei transfers land on top in unknown
    // number, so this is a floor, not an equality).
    assert!(
        en.l2_provider.get_balance(recipient).await? >= value + value,
        "pre- and post-cutover effects both serve",
    );

    Ok(())
}

#[test_multisetup([CURRENT_TO_L1])]
async fn a_cutover_scheduled_below_the_tip_refuses_to_start(
    main_node: Tester,
) -> anyhow::Result<()> {
    // Two blocks of history, so an anchor at the first one sits strictly below
    // the tip while still having a write-ahead-log record.
    let recipient = Address::repeat_byte(0x65);
    let (first_block, _) = transfer_via(&main_node, recipient, U256::from(1u64)).await?;
    let (second_block, _) = transfer_via(&main_node, recipient, U256::from(1u64)).await?;
    anyhow::ensure!(second_block > first_block, "two blocks of history");

    let seat = CommitteeSeat::reserve().await?;
    let seat_peer = CommitteeSeat::reserve().await?;
    let committee = vec![seat.committee_entry(), seat_peer.committee_entry()];

    // The chain already sequenced past the anchor — the config landed too late.
    // The node must refuse rather than truncate or re-anchor on its own.
    let stopped = main_node.stop().await?;
    let backup = stopped.backup();
    let refused = stopped
        .start_with_overrides(|config| {
            seat.arm_consensus(config, committee.clone(), first_block);
            // No external nodes take part here, and a refused start must not
            // leave a devp2p service holding the databases past the relaunch
            // below (the network service is spawned before the era guard).
            config.network_config.enabled = false;
        })
        .await;
    let error = refused.expect_err("a chain past the anchor must refuse to start consensus");
    assert!(
        error
            .to_string()
            .contains("must start exactly at the agreed cutover"),
        "unexpected refusal: {error:#}",
    );

    // The refusal leaves the chain intact, and the documented remedy — schedule
    // a fresh anchor above the tip — arms the cutover on the same databases.
    let node = backup
        .restore()
        .await?
        .start_with_overrides(|config| {
            seat.arm_consensus(config, committee.clone(), second_block + 50);
            config.network_config.enabled = false;
        })
        .await?;
    assert!(
        node.scheduled_cutover_reached.is_some(),
        "a future anchor arms the scheduled cutover",
    );
    let pending = node
        .status()
        .await?
        .scheduled_cutover
        .expect("the pending cutover surfaces in /status");
    assert_eq!(pending.genesis_height, second_block + 50);

    Ok(())
}

#[test_multisetup([CURRENT_TO_L1])]
async fn migrated_chain_rolls_back_to_the_single_sequencer(
    main_node: Tester,
) -> anyhow::Result<()> {
    let recipient = Address::repeat_byte(0x62);
    let value = U256::from(3_000_000u64);

    // Cutover, then some consensus-era history worth preserving.
    let stopped = main_node.stop().await?;
    let mut cluster = MultiNodeTester::migrate_from(&stopped, 3).await?;
    let (consensus_era_block, consensus_era_tx) =
        transfer_via(cluster.node(1), recipient, value).await?;
    cluster
        .wait_for_block_on_all(consensus_era_block, CONVERGENCE_TIMEOUT)
        .await?;
    let consensus_tip = cluster.max_height().await?;

    // Rollback: stop the whole committee, then restart the batcher validator as a
    // plain single sequencer — same databases, consensus disabled, the rollback
    // explicitly acknowledged (without the flag the node refuses; that guard is
    // unit-pinned). The settler goes down first: its final commit may still be
    // in flight, and if it lands only after the relaunched node's startup L1
    // snapshot, the unexpected-commit guard reads the node's own commit as a
    // foreign settler's and dies. Stopping the other two validators in between
    // gives L1 that much longer to digest it before the relaunch.
    for index in [0, 1, 2] {
        cluster.stop_validator(index).await?;
    }
    cluster
        .start_validator_with_config_overrides(0, |config| {
            config.consensus_config.enabled = false;
            config.consensus_config.acknowledge_rollback = true;
        })
        .await?;

    // The single sequencer produces on demand (it does not seal empty blocks the
    // way the consensus cadence does), so drive it with traffic. Note the resumed
    // chain continues from THIS node's durable tip, which may be a block or two
    // behind the highest height consensus ever finalized: finality (a certificate
    // exists) runs slightly ahead of durability (every validator has committed the
    // block). A real rollback picks the validator with the highest write-ahead-log
    // tip and accepts that the in-flight tail may be dropped — the runbook rule
    // this test documents.
    let post_rollback_recipient = Address::repeat_byte(0x63);
    let (post_rollback_block, _) =
        transfer_via(cluster.node(0), post_rollback_recipient, value).await?;
    assert!(
        post_rollback_block > consensus_era_block,
        "linear production must continue past the durable consensus era          ({post_rollback_block} vs {consensus_era_block}; consensus tip was {consensus_tip})",
    );

    // The consensus era's durable blocks are ordinary chain history to the single
    // sequencer: receipts and state effects are served unchanged.
    assert_eq!(
        cluster.node(0).l2_provider.get_balance(recipient).await?,
        value,
        "the consensus era's state must survive the rollback",
    );
    assert!(
        cluster
            .node(0)
            .l2_provider
            .get_transaction_receipt(consensus_era_tx)
            .await?
            .is_some(),
        "the consensus era's receipts must survive the rollback",
    );

    cluster.shutdown_all().await
}
