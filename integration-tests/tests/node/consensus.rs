//! BFT consensus over real nodes: committees of full in-process nodes over one L1,
//! producing, verifying, and finalizing real blocks — including validators stopping
//! and rejoining.
//!
//! Reaching the first assertion of any test here already proves a lot: node startup
//! waits for the initial L1 deposit to be *included in a block*, which under consensus
//! requires the committee to form over p2p, a leader to build a block carrying the L1
//! priority transaction, the other validators to re-execute and vote for it, and the
//! finalized block to flow through every node's persistence pipeline.

use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use std::time::Duration;
use zksync_os_integration_tests::l1_helpers::wait_for_l1_state;
use zksync_os_integration_tests::multi_node::MultiNodeTester;
use zksync_os_integration_tests::settlement::{CONVERGENCE_TIMEOUT, send_transfer};

#[test_log::test(tokio::test)]
async fn three_validators_finalize_and_agree() -> anyhow::Result<()> {
    let cluster = MultiNodeTester::start(3).await?;

    // A user transaction, submitted to a non-batcher validator: it sits in that
    // validator's mempool until that validator's turn as leader, then rides a block
    // like any other transaction. (`send_transfer` moves 1_000_000 wei — the value
    // the balance assertion below checks for.)
    let recipient = Address::repeat_byte(0x42);
    let value = U256::from(1_000_000u64);
    let included_at = send_transfer(&cluster, 1, recipient).await?;

    // Every validator converges on the same chain: same height reached, identical
    // block hash where the transaction landed, and the state effect visible everywhere.
    cluster
        .wait_for_block_on_all(included_at, CONVERGENCE_TIMEOUT)
        .await?;
    cluster.assert_block_hashes_agree(included_at).await?;
    for index in 0..cluster.len() {
        let balance = cluster
            .node(index)
            .l2_provider
            .get_balance(recipient)
            .await?;
        assert_eq!(balance, value, "validator {index} sees a different balance");
    }

    // Liveness: the chain keeps growing past the transaction.
    cluster
        .wait_for_block_on_all(included_at + 5, CONVERGENCE_TIMEOUT)
        .await?;

    // The observability surfaces external monitors poll: `/status` reports consensus
    // progress, and the consensus runtime's own registry is served for scraping.
    let status = cluster.node(1).status().await?;
    let consensus = status
        .consensus
        .expect("validators must report a consensus status section");
    assert_eq!(consensus.committee_size, 3);
    let finalized = consensus
        .finalized
        .expect("a finalized round must have been observed by now");
    assert!(finalized.view > 0, "finalized view must have advanced");
    assert!(
        consensus.applied_height.unwrap_or(0) >= included_at,
        "applied height must cover the included transaction",
    );
    let metrics = cluster.node(1).consensus_metrics().await?;
    assert!(
        !metrics.is_empty(),
        "the consensus runtime's metrics registry must serve content",
    );

    // The sovereign finality trail: every finalized block's certificate is converted
    // into the node's own store the moment it is observed. The certified watermark
    // covering the transaction's block proves both write paths — certificates from
    // the activity observer, the height index from the commit path — are live and
    // joining correctly, end to end.
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
            "finality certificates did not cover block {included_at} within \
             {CONVERGENCE_TIMEOUT:?} (certified so far: {certified:?})",
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    cluster.shutdown_all().await
}

/// A validator that restarts on its own state must rejoin the committee, backfill the
/// blocks it missed, and *participate* again — the full restart surface in one flow:
/// consensus journal replay, the committed-tip digest re-anchor, cold-path commits
/// (re-execution of backfilled blocks), the mempool re-init, and the L1 watcher
/// re-scan that verification's input authenticity depends on.
#[test_log::test(tokio::test)]
async fn validator_restart_rejoins_catches_up_and_votes_again() -> anyhow::Result<()> {
    // Four validators: a committee that tolerates one fault, so the chain keeps
    // running while one validator is down.
    let mut cluster = MultiNodeTester::start(4).await?;

    let included_at = send_transfer(&cluster, 1, Address::repeat_byte(0x51)).await?;
    cluster
        .wait_for_block_on_all(included_at, CONVERGENCE_TIMEOUT)
        .await?;

    // Take a follower down; the remaining three are exactly quorum and keep
    // finalizing (this validator's leader turns simply time out).
    cluster.stop_validator(3).await?;
    let while_down = send_transfer(&cluster, 1, Address::repeat_byte(0x52)).await?;
    cluster
        .wait_for_block_on_all(while_down + 3, CONVERGENCE_TIMEOUT)
        .await?;

    // Restart it on its original state and keys. It must catch up to the live tip —
    // which keeps moving — and serve the blocks it missed identically.
    cluster.start_validator(3).await?;
    let target = cluster.max_height().await? + 3;
    cluster
        .wait_for_block_on_all(target, CONVERGENCE_TIMEOUT)
        .await?;
    cluster.assert_block_hashes_agree(while_down).await?;

    // Catching up proves it *follows*; now prove it *participates*. With a different
    // validator stopped, the remaining three are exactly quorum again — every new
    // finalization now requires the restarted validator's vote, and its vote requires
    // it to verify fresh proposals (validity rules + re-execution) on post-restart
    // state.
    cluster.stop_validator(1).await?;
    let after_rejoin = send_transfer(&cluster, 2, Address::repeat_byte(0x53)).await?;
    cluster
        .wait_for_block_on_all(after_rejoin, CONVERGENCE_TIMEOUT)
        .await?;
    cluster.assert_block_hashes_agree(after_rejoin).await?;

    // The finality trail must self-heal too: certificates for the heights
    // finalized while validator 3 was down never re-broadcast, but its first
    // live certificate after catching up covers them — the certified watermark
    // converges to the moving tip instead of stalling at the downtime hole.
    let deadline = tokio::time::Instant::now() + CONVERGENCE_TIMEOUT;
    loop {
        let status = cluster.node(3).status().await?;
        let consensus = status.consensus.expect("validator serves consensus status");
        let certified = consensus.finality_certified_height.unwrap_or(0);
        let applied = consensus.applied_height.unwrap_or(0);
        if certified >= after_rejoin && applied.saturating_sub(certified) < 16 {
            break;
        }
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "restarted validator's certified watermark never converged: \
             certified {certified}, applied {applied}, needed >= {after_rejoin}",
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    cluster.shutdown_all().await
}

/// The whole committee must survive an L1 outage — degrade, never die. Every
/// validator loses its L1 RPC at once (the shared-provider-outage shape, and the
/// chaos rig's partition fault), for longer than the provider-level retry budget
/// that used to be the only buffer before background components crashed the node.
/// While L1 is away the chain must keep finalizing L2 traffic; when L1 returns,
/// settlement must resume on its own. Found by the chaos rig's third soak (three
/// distinct death mechanisms: the upgrade gatekeeper, the L1 senders, and the
/// provider's own header pollers).
///
/// Four validators, deliberately: the batcher validator's pipeline intentionally
/// pauses on its bounded runway once settlement backs up (the speculative-state
/// cap), which makes that one node withhold votes until L1 returns — the committee
/// must ride through that like any single-node fault, which needs n >= 4.
#[test_log::test(tokio::test)]
async fn committee_survives_l1_outage() -> anyhow::Result<()> {
    let (cluster, mut l1_proxy) = MultiNodeTester::start_with_severable_l1(4).await?;

    // Healthy baseline: real traffic and at least one batch committed on L1, so the
    // L1 senders are mid-flight when the outage begins.
    let included_at = send_transfer(&cluster, 1, Address::repeat_byte(0x81)).await?;
    cluster
        .wait_for_block_on_all(included_at, CONVERGENCE_TIMEOUT)
        .await?;
    let before_outage = wait_for_l1_state(cluster.node(0), "a batch committed on L1", |state| {
        state.last_committed_batch >= 1
    })
    .await?;

    // The outage: refuse every L1 connection for well past the provider's internal
    // retry budget (~40s was enough to kill a node before the fix).
    l1_proxy.sever().await;
    tokio::time::sleep(Duration::from_secs(60)).await;

    // Mid-outage, the committee must still be finalizing: an L2 transfer needs no
    // L1 input and must land normally.
    let during_outage = send_transfer(&cluster, 1, Address::repeat_byte(0x82)).await?;
    cluster
        .wait_for_block_on_all(during_outage, CONVERGENCE_TIMEOUT)
        .await?;
    cluster.assert_block_hashes_agree(during_outage).await?;
    for index in 0..cluster.len() {
        let status = cluster.node(index).status().await?;
        anyhow::ensure!(
            status.healthy,
            "validator {index} reports unhealthy during the L1 outage"
        );
    }

    // L1 returns: settlement must resume without any restart.
    l1_proxy.restore().await?;
    wait_for_l1_state(
        cluster.node(0),
        "batch settlement resumes after the outage",
        |state| state.last_committed_batch > before_outage.last_committed_batch,
    )
    .await?;

    cluster.shutdown_all().await
}

/// The batcher validator must survive a restart. Its recovery is the strictest in the
/// committee: the batcher resumes from the last L1-*executed* batch and re-creates
/// every batch already committed on L1, which requires the pipeline to replay blocks
/// from that point — strictly below the WAL tip where the live consensus stream
/// resumes. Found by the chaos rig's first soak: consensus mode fed the pipeline only
/// live finalized blocks, and the restarted batcher died on the gap ("Existing batch
/// first block (N) does not match next block in stream (M)").
#[test_log::test(tokio::test)]
async fn batcher_validator_restart_recreates_batches_and_keeps_settling() -> anyhow::Result<()> {
    // Four validators: the chain keeps finalizing while the batcher validator is
    // down, so it restarts into a moving committee, replays its own write-ahead log,
    // and backfills the rest — the exact shape of the chaos-rig failure.
    let mut cluster = MultiNodeTester::start(4).await?;

    // Real batches on L1: wait until a batch is committed whose execution has not
    // landed yet. That is precisely the state whose recovery was broken — and it
    // stays that way while the batcher validator is down, because both the commit
    // and the execute senders live on that node.
    let included_at = send_transfer(&cluster, 1, Address::repeat_byte(0x61)).await?;
    cluster
        .wait_for_block_on_all(included_at, CONVERGENCE_TIMEOUT)
        .await?;
    let before_restart = wait_for_l1_state(
        cluster.node(0),
        "a batch committed but not yet executed on L1",
        |state| state.last_committed_batch > state.last_executed_batch,
    )
    .await?;

    // Restart the batcher validator while that recovery window is open; the chain
    // moves on without it in the meantime.
    cluster.stop_validator(0).await?;
    let while_down = send_transfer(&cluster, 1, Address::repeat_byte(0x62)).await?;
    cluster
        .wait_for_block_on_all(while_down, CONVERGENCE_TIMEOUT)
        .await?;
    cluster.start_validator(0).await?;

    // The restarted node must replay, catch up to the moving tip, and agree.
    let target = cluster.max_height().await? + 3;
    cluster
        .wait_for_block_on_all(target, CONVERGENCE_TIMEOUT)
        .await?;
    cluster.assert_block_hashes_agree(while_down).await?;

    // The sharp assertion: the batcher is alive *past its recovery* — new batches
    // land on L1 after the restart. Before the restart-replay fix the batcher task
    // panicked within seconds of startup and settlement stopped for good.
    wait_for_l1_state(
        cluster.node(0),
        "the restarted batcher commits new batches to L1",
        |state| state.last_committed_batch > before_restart.last_committed_batch,
    )
    .await?;

    cluster.shutdown_all().await
}

/// A three-validator simplex committee tolerates zero faults (quorum is 3-of-3):
/// stopping any validator pauses finalization, and restarting it resumes the chain.
/// This pins the availability boundary as an executable fact — deployments that need
/// to survive f faults must size the committee at n >= 3f+1, i.e. at least four.
#[test_log::test(tokio::test)]
async fn three_validator_chain_pauses_without_quorum_and_resumes() -> anyhow::Result<()> {
    let mut cluster = MultiNodeTester::start(3).await?;

    cluster.stop_validator(2).await?;

    // Fully-formed finalizations may still deliver right after the stop; once those
    // settle, nothing new can finalize — a new certificate needs all three votes.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let frozen_at = cluster.max_height().await?;
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert_eq!(
        cluster.max_height().await?,
        frozen_at,
        "the chain advanced without quorum",
    );

    // The committee is whole again: the chain must pick up where it froze.
    cluster.start_validator(2).await?;
    cluster
        .wait_for_block_on_all(frozen_at + 5, CONVERGENCE_TIMEOUT)
        .await?;

    cluster.shutdown_all().await
}

/// A transaction must reach a leader no matter which validator's RPC received it: the
/// committee gossips its mempools. The setup makes gossip the only possible carrier —
/// a nonce-gapped transaction (not yet includable) is submitted to one validator, that
/// validator dies, and only then is the gap filled through a different validator. The
/// gapped transaction's inclusion proves it left the dead validator's mempool over
/// gossip; nothing else ever had its bytes.
#[test_log::test(tokio::test)]
async fn gossiped_transaction_survives_its_receiving_validator() -> anyhow::Result<()> {
    let mut cluster = MultiNodeTester::start(4).await?;
    let sender = cluster.node(3).l2_wallet.default_signer().address();
    let base_nonce = cluster
        .node(3)
        .l2_provider
        .get_transaction_count(sender)
        .await?;

    // The gapped transaction: queued everywhere until `base_nonce` is used, so no
    // leader can include it while validator 3 is still alive. Gas is set explicitly —
    // estimation would simulate with the gapped nonce and refuse.
    let gapped = cluster
        .node(3)
        .l2_provider
        .send_transaction(
            TransactionRequest::default()
                .with_to(Address::repeat_byte(0x61))
                .with_value(U256::from(1u64))
                .with_nonce(base_nonce + 1)
                .with_gas_limit(210_000),
        )
        .await?;
    let gapped_hash = *gapped.tx_hash();

    // Gossip carries it to the rest of the committee (visible in their pools).
    let deadline = tokio::time::Instant::now() + CONVERGENCE_TIMEOUT;
    loop {
        if cluster
            .node(0)
            .l2_provider
            .get_transaction_by_hash(gapped_hash)
            .await?
            .is_some()
        {
            break;
        }
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "transaction did not gossip to other validators within {CONVERGENCE_TIMEOUT:?}",
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // The only validator whose RPC ever saw the transaction goes away...
    cluster.stop_validator(3).await?;

    // ...and once the gap is filled through a different validator, the gossiped copy
    // becomes includable and lands. The gap-filler's own receipt is not awaited —
    // the gapped transaction's receipt below proves both were included, in order.
    let _ = cluster
        .node(0)
        .l2_provider
        .send_transaction(
            TransactionRequest::default()
                .with_to(Address::repeat_byte(0x62))
                .with_value(U256::from(1u64))
                .with_nonce(base_nonce)
                .with_gas_limit(210_000),
        )
        .await?;

    let deadline = tokio::time::Instant::now() + CONVERGENCE_TIMEOUT;
    let receipt = loop {
        if let Some(receipt) = cluster
            .node(0)
            .l2_provider
            .get_transaction_receipt(gapped_hash)
            .await?
        {
            break receipt;
        }
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "gossiped transaction was not included within {CONVERGENCE_TIMEOUT:?} \
             after its receiving validator died",
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert!(receipt.status(), "gossiped transaction must succeed");

    let included_at = receipt
        .block_number
        .expect("included transactions have a block");
    cluster
        .wait_for_block_on_all(included_at, CONVERGENCE_TIMEOUT)
        .await?;
    cluster.assert_block_hashes_agree(included_at).await?;

    cluster.shutdown_all().await
}

/// Waits until a single node's RPC serves at least block `height`.
async fn wait_for_block_on(
    node: &zksync_os_integration_tests::Tester,
    height: u64,
    timeout: Duration,
) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if node.l2_provider.get_block_number().await.unwrap_or(0) >= height {
            return Ok(());
        }
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "node did not reach block {height} within {timeout:?}",
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// External nodes are not committee members: they follow the finalized chain over the
/// same replay protocol they use against a single sequencer, pointed at any validator
/// of their choice. Consensus must leave that contract untouched — an EN needs no
/// consensus keys, no committee membership, and no awareness that its upstream is one
/// of several validators.
#[test_log::test(tokio::test)]
async fn external_node_syncs_from_a_consensus_validator() -> anyhow::Result<()> {
    let cluster = MultiNodeTester::start(3).await?;

    // Build history *before* the EN exists, so its sync starts with pure catch-up.
    let recipient = Address::repeat_byte(0x51);
    let value = U256::from(1_000_000u64);
    let included_at = send_transfer(&cluster, 1, recipient).await?;
    cluster
        .wait_for_block_on_all(included_at, CONVERGENCE_TIMEOUT)
        .await?;

    // The EN follows a NON-batcher validator: the replay stream is a property of
    // every validator, not of the settlement node.
    let en = cluster.node(1).launch_external_node().await?;

    // Catch-up: the EN replays the pre-existing history and serves the same chain.
    wait_for_block_on(&en, included_at, CONVERGENCE_TIMEOUT).await?;
    {
        use alloy::eips::BlockId;
        let en_block = en
            .l2_provider
            .get_block(BlockId::number(included_at))
            .await?
            .expect("the EN just reported this height");
        let validator_block = cluster
            .node(1)
            .l2_provider
            .get_block(BlockId::number(included_at))
            .await?
            .expect("the validator served this block already");
        assert_eq!(
            en_block.header.hash, validator_block.header.hash,
            "the EN synced a different block {included_at} than the committee finalized",
        );
    }
    let balance = en.l2_provider.get_balance(recipient).await?;
    assert_eq!(balance, value, "the EN replayed to a different state");

    // Live tail: traffic that happens after the EN joined keeps streaming to it.
    let second_at = send_transfer(&cluster, 2, recipient).await?;
    wait_for_block_on(&en, second_at, CONVERGENCE_TIMEOUT).await?;
    let balance = en.l2_provider.get_balance(recipient).await?;
    assert_eq!(
        balance,
        value + value,
        "the EN did not follow the chain past its join point",
    );

    en.shutdown().await?;
    cluster.shutdown_all().await
}

/// The committee protocol version is part of the network's identity: a validator
/// configured with a different version must fail to pair at the handshake — cleanly
/// and loudly — rather than exchange messages the committee would interpret
/// differently. A committee of n >= 3f+1 rides through it as a one-member fault;
/// deploying a binary that *supports* a new version is thus always safe, and only
/// the coordinated activation (flipping the config) changes behavior.
#[test_log::test(tokio::test)]
async fn validator_on_a_different_protocol_version_cannot_pair() -> anyhow::Result<()> {
    let mut cluster = MultiNodeTester::start(4).await?;

    // A healthy baseline: the full committee applies the same chain.
    let recipient = Address::repeat_byte(0x77);
    let included_at = send_transfer(&cluster, 1, recipient).await?;
    cluster
        .wait_for_block_on_all(included_at, CONVERGENCE_TIMEOUT)
        .await?;

    // Validator 3 comes back believing the committee speaks protocol version 2.
    let height_before_restart = {
        use alloy::providers::Provider as _;
        cluster.node(3).l2_provider.get_block_number().await?
    };
    cluster.stop_validator(3).await?;
    cluster
        .start_validator_with_config_overrides(3, |config| {
            config.consensus_config.protocol_version = 2;
        })
        .await?;

    // The remaining three are exactly quorum: the committee is unimpaired.
    let second_at = send_transfer(&cluster, 0, recipient).await?;
    for index in 0..3 {
        wait_for_block_on(cluster.node(index), second_at, CONVERGENCE_TIMEOUT).await?;
    }

    // What "cannot pair" observably means: the validator's consensus knowledge
    // freezes where its own disk left off. (Its journal replay re-reports rounds it
    // finalized *before* the restart, so merely "observed a finalization" proves
    // nothing — the sharp fact is that the committee's finalized view keeps
    // advancing while the mismatched validator's stands still.)
    fn finalized_view(status: zksync_os_status_server::StatusResponse) -> u64 {
        status
            .consensus
            .and_then(|consensus| consensus.finalized)
            .map(|finalized| finalized.view)
            .unwrap_or(0)
    }
    // The replay's pace depends on host load, so record the frozen view and
    // height only once both have held still for a while — a fixed sleep
    // undershoots on a busy machine and the assertions below then read replay
    // progress as a pairing leak. If they never settle, that IS the pairing
    // leak, reported here.
    let (stuck_view, stuck_height) = {
        use alloy::providers::Provider as _;
        let observe = || async {
            anyhow::Ok((
                finalized_view(cluster.node(3).status().await?),
                cluster.node(3).l2_provider.get_block_number().await?,
            ))
        };
        let deadline = tokio::time::Instant::now() + CONVERGENCE_TIMEOUT;
        let mut last = observe().await?;
        let mut stable_since = tokio::time::Instant::now();
        loop {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let now = observe().await?;
            if now != last {
                last = now;
                stable_since = tokio::time::Instant::now();
            } else if stable_since.elapsed() >= Duration::from_secs(2) {
                break last;
            }
            anyhow::ensure!(
                tokio::time::Instant::now() < deadline,
                "the mismatched validator's consensus knowledge never settled \
                 (still moving at view {}, height {})",
                now.0,
                now.1,
            );
        }
    };

    // Let the committee finalize well past everything the mismatched validator knows.
    let deadline = tokio::time::Instant::now() + CONVERGENCE_TIMEOUT;
    loop {
        let committee_view = finalized_view(cluster.node(0).status().await?);
        if committee_view >= stuck_view + 40 {
            break;
        }
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "the committee's finalized view did not advance past the mismatched \
             validator's ({committee_view} vs {stuck_view})",
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    let view_now = finalized_view(cluster.node(3).status().await?);
    assert_eq!(
        view_now, stuck_view,
        "a protocol-mismatched validator must not observe new finalizations",
    );
    let height_after = {
        use alloy::providers::Provider as _;
        cluster.node(3).l2_provider.get_block_number().await?
    };
    assert_eq!(
        height_after, stuck_height,
        "a protocol-mismatched validator must not apply new blocks",
    );
    assert!(
        second_at > height_before_restart,
        "sanity: the committee must have moved past the mismatched validator",
    );

    cluster.shutdown_all().await
}

/// A non-voting observer on the consensus network: it holds no BLS key, appears in
/// no committee, and has no serving node configured (`main_node_rpc_url` unset) —
/// everything it believes arrives as gossiped blocks with finality certificates it
/// verifies against the committee schedule. Its RPC must still be fully usable:
/// a transaction submitted to the observer is forwarded to a validator, gossiped
/// to the leader, included, finalized — and the observer serves the receipt from
/// its own (consensus-verified) chain.
#[test_log::test(tokio::test)]
async fn observer_follows_the_committee_and_serves_transactions() -> anyhow::Result<()> {
    let cluster = MultiNodeTester::start_with_observers(3, 1).await?;
    const OBSERVER: usize = 3;

    // The observer's whole startup already proves following: reaching here means it
    // applied the initial-deposit block it could only have received via consensus.
    // Now the RPC path: submit through the observer, get the receipt from it.
    let included_at = send_transfer(&cluster, OBSERVER, Address::repeat_byte(0x51)).await?;

    // Everyone — observer included — converges on the block, byte-identical.
    cluster
        .wait_for_block_on_all(included_at, CONVERGENCE_TIMEOUT)
        .await?;
    cluster.assert_block_hashes_agree(included_at).await?;

    // The status surface tells the roles apart, and the observer's view of
    // finality is live (its scout verifies certificates for epochs it runs no
    // engine in — that observation is what keeps this field advancing).
    let observer_status = cluster.node(OBSERVER).status().await?;
    let consensus = observer_status
        .consensus
        .expect("observer serves a consensus status section");
    assert_eq!(consensus.role, "observer");
    assert!(
        consensus.finalized.is_some(),
        "observer never observed a finalization"
    );
    let validator_status = cluster.node(0).status().await?;
    assert_eq!(
        validator_status
            .consensus
            .expect("validator serves a consensus status section")
            .role,
        "validator"
    );

    Ok(())
}

/// The idle policy end to end on real nodes: a quiet chain stops producing
/// blocks, one heartbeat bounds the silence, and a transaction wakes the chain
/// promptly. (Other consensus tests pin the legacy always-build behavior via
/// the test config; this one opts into heartbeats explicitly.)
#[test_log::test(tokio::test)]
async fn an_idle_chain_heartbeats_and_wakes_on_work() -> anyhow::Result<()> {
    const HEARTBEAT: Duration = Duration::from_secs(4);
    let cluster = MultiNodeTester::start_with_config_overrides(2, |config| {
        config.consensus_config.idle_heartbeat = HEARTBEAT;
    })
    .await?;

    // Real work makes a block promptly.
    send_transfer(&cluster, 0, Address::repeat_byte(0x61)).await?;
    let after_work = cluster.node(0).l2_provider.get_block_number().await?;

    // Quiet: well inside the heartbeat interval the chain must not grow by
    // more than the one heartbeat that may have been mid-flight. (The legacy
    // behavior would add ~4 blocks per second here.)
    tokio::time::sleep(HEARTBEAT / 2).await;
    let mid_quiet = cluster.node(0).l2_provider.get_block_number().await?;
    assert!(
        mid_quiet <= after_work + 1,
        "an idle chain kept producing: {after_work} -> {mid_quiet} within half a heartbeat",
    );

    // Across a couple of intervals the chain grows by heartbeats, not cadence:
    // strictly more than zero, far fewer than the legacy flood.
    tokio::time::sleep(HEARTBEAT * 2).await;
    let after_quiet = cluster.node(0).l2_provider.get_block_number().await?;
    assert!(
        after_quiet > mid_quiet,
        "no heartbeat inside {HEARTBEAT:?} x2 of quiet",
    );
    assert!(
        after_quiet <= mid_quiet + 3,
        "too many blocks for a heartbeated quiet window: {mid_quiet} -> {after_quiet}",
    );

    // Work still wakes the chain immediately.
    let woke_at = send_transfer(&cluster, 1, Address::repeat_byte(0x62)).await?;
    assert!(woke_at > after_quiet);

    Ok(())
}

/// The zksync-os protocol upgrade, end to end on a live committee — the
/// choreography every prior test exercised only on a single node: governance
/// lands the upgrade on L1, every validator's watcher gates it independently,
/// some leader includes the upgrade transaction, the *other* validators verify
/// its content byte-for-byte against their own L1 view before voting, and the
/// activation flips execution semantics on all of them in the same block.
/// A divergence anywhere in that chain is exactly what the verify-before-vote
/// alarm exists to catch — this test is the drill that keeps a real upgrade
/// from being the first time the composition runs.
#[test_log::test(tokio::test)]
async fn protocol_upgrade_executes_under_consensus() -> anyhow::Result<()> {
    use alloy::primitives::{Bytes, FixedBytes};
    use alloy::sol_types::SolCall as _;
    use std::collections::BTreeMap;
    use zksync_os_integration_tests::contracts::SampleForceDeployment;
    use zksync_os_integration_tests::upgrade::{
        Action, CommitterFacetV31, FacetCut, UpgradeTester,
    };

    let cluster = MultiNodeTester::start(4).await?;

    // A healthy pre-upgrade baseline — including agreement on the
    // committee-uniform config fingerprint (these nodes were configured by
    // one harness, so any mismatch is a fingerprint bug).
    let before = send_transfer(&cluster, 1, Address::repeat_byte(0x71)).await?;
    cluster
        .wait_for_block_on_all(before, CONVERGENCE_TIMEOUT)
        .await?;
    let reference = cluster
        .node(0)
        .status()
        .await?
        .consensus
        .expect("consensus status")
        .chain_fingerprint;
    assert!(!reference.is_empty(), "fingerprint must be served");
    for validator in 1..4 {
        let fingerprint = cluster
            .node(validator)
            .status()
            .await?
            .consensus
            .expect("consensus status")
            .chain_fingerprint;
        assert_eq!(
            fingerprint, reference,
            "validator {validator} serves a different chain fingerprint",
        );
    }

    // Governance is pure L1: drive it through node 0's provider — the shared
    // anvil means every validator's watcher sees the same upgrade.
    let upgrade_tester = UpgradeTester::for_default_upgrade(cluster.node(0)).await?;
    upgrade_tester
        .publish_bytecodes([SampleForceDeployment::BYTECODE.clone()])
        .await?;

    let force_address: Address = "0x000000000000000000000000000000000000dead".parse()?;
    let force_deployments: BTreeMap<Address, Bytes> = [(
        force_address,
        SampleForceDeployment::DEPLOYED_BYTECODE.clone(),
    )]
    .into_iter()
    .collect();
    let protocol_upgrade = upgrade_tester
        .protocol_upgrade_builder()
        .await?
        .bump_minor(1)
        .with_force_deployments(force_deployments)
        .with_timestamp(U256::from(1))
        .build();

    let l1_chain_id = cluster.node(0).l1_provider().get_chain_id().await?;
    let committer_facet = CommitterFacetV31::deploy(
        cluster.node(0).l1_provider().clone(),
        U256::from(l1_chain_id),
    )
    .await?;
    let facet_cut = FacetCut {
        facet: *committer_facet.address(),
        action: Action::Replace,
        isFreezable: true,
        selectors: vec![FixedBytes(
            CommitterFacetV31::commitBatchesSharedBridgeCall::SELECTOR,
        )],
    };

    upgrade_tester
        .execute_default_upgrade(
            &protocol_upgrade,
            U256::MAX,
            U256::from(1),
            false,
            vec![facet_cut],
        )
        .await?;

    // The activation executed *identically* on every validator: the force
    // deployment is post-upgrade state, served by each node's own chain.
    for validator in 0..4 {
        let deployed =
            SampleForceDeployment::new(force_address, cluster.node(validator).l2_provider.clone());
        assert_eq!(
            deployed.return42().call().await?,
            U256::from(42),
            "validator {validator} did not execute the upgrade's force deployment",
        );
    }

    // The committee agreed on the upgrade-era blocks, and stays live with
    // every validator accepting traffic post-upgrade.
    let tip = cluster.max_height().await?;
    cluster
        .wait_for_block_on_all(tip, CONVERGENCE_TIMEOUT)
        .await?;
    cluster.assert_block_hashes_agree(tip).await?;
    let mut last = tip;
    for via in [1, 2, 3] {
        last = send_transfer(&cluster, via, Address::repeat_byte(0x72 + via as u8)).await?;
        // Converge before rotating to the next submitter: each node's provider
        // derives the sender nonce from that node's own state, so a lagging
        // follower would compute a stale nonce.
        cluster
            .wait_for_block_on_all(last, CONVERGENCE_TIMEOUT)
            .await?;
    }
    cluster.assert_block_hashes_agree(last).await?;

    Ok(())
}
