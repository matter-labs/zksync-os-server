//! The committee as the batch-verification (2FA) set: every validator already
//! re-executed every block before voting for it, so co-signing the settler's
//! batch commitment costs a recomputation from local finalized data — no
//! separate verifier fleet, and the signature attests against *independent*
//! BFT finality instead of a chain synced from the node being checked.
//!
//! Mechanically: validators mesh on the zks network with stable identities,
//! every standby runs the batch-verification responder, and the settler
//! collects `threshold` signatures before its commit lands on L1 (the multisig
//! committer path). The settler never co-signs its own batches, so thresholds
//! must be reachable from the standbys alone: `threshold <= n - 1 - f`.

use std::time::Duration;
use zksync_os_integration_tests::l1_helpers::{
    fetch_l1_state, wait_for_l1_state, wait_for_l1_state_with_timeout,
};

/// The multisig ladder is legitimately slower than plain settlement: every batch
/// adds a signature collection round-trip, and the first batches pay the
/// verifier-handshake warm-up. Budget accordingly.
const SETTLEMENT_LADDER_TIMEOUT: Duration = Duration::from_secs(360);
use zksync_os_integration_tests::multi_node::MultiNodeTester;
use zksync_os_integration_tests::settlement::{
    CONVERGENCE_TIMEOUT, SettlerIdentity, authorize_and_fund, send_transfer,
};

/// The happy path: a four-validator committee where the three standbys co-sign
/// and the settler needs two of them per batch. Settlement advancing to
/// *executed* proves the whole ladder ran multisig: with the threshold unmet,
/// commits never land (the negative test below pins that this is not vacuous).
#[test_log::test(tokio::test)]
async fn committee_co_signs_batches_and_settlement_advances() -> anyhow::Result<()> {
    let cluster = MultiNodeTester::start_with_batch_verification(4, 2).await?;

    let included_at =
        send_transfer(&cluster, 1, alloy::primitives::Address::repeat_byte(0x81)).await?;
    cluster
        .wait_for_block_on_all(included_at, CONVERGENCE_TIMEOUT)
        .await?;

    wait_for_l1_state_with_timeout(
        cluster.node(1),
        "a co-signed batch is committed and executed on L1",
        SETTLEMENT_LADDER_TIMEOUT,
        |state| state.last_executed_batch > 0,
    )
    .await?;

    cluster.shutdown_all().await
}

/// The sharp negative: with a threshold no set of standbys can satisfy, the
/// settler must refuse to commit — batches stall rather than land unverified.
/// This is what makes the happy path meaningful: signatures are load-bearing.
#[test_log::test(tokio::test)]
async fn unreachable_threshold_stalls_settlement_instead_of_bypassing_it() -> anyhow::Result<()> {
    // Three standbys can produce at most three signatures.
    let cluster = MultiNodeTester::start_with_batch_verification(4, 4).await?;

    let included_at =
        send_transfer(&cluster, 1, alloy::primitives::Address::repeat_byte(0x82)).await?;
    cluster
        .wait_for_block_on_all(included_at, CONVERGENCE_TIMEOUT)
        .await?;

    // Sequencing is unaffected — blocks finalize — but nothing may reach L1.
    tokio::time::sleep(Duration::from_secs(20)).await;
    let state = fetch_l1_state(cluster.node(1)).await?;
    anyhow::ensure!(
        state.last_committed_batch == 0,
        "batch {} was committed without the required signatures",
        state.last_committed_batch,
    );

    cluster.shutdown_all().await
}

/// Failover under 2FA: the settler dies, a promoted standby takes over — and
/// the *verifier set follows automatically*, because every validator is a
/// verifier and the new settler collects from its own committee sessions. The
/// recreated batches are re-signed by the surviving standbys.
#[test_log::test(tokio::test)]
async fn a_promoted_settler_keeps_collecting_signatures() -> anyhow::Result<()> {
    let mut cluster = MultiNodeTester::start_with_batch_verification(4, 2).await?;

    let included_at =
        send_transfer(&cluster, 1, alloy::primitives::Address::repeat_byte(0x83)).await?;
    cluster
        .wait_for_block_on_all(included_at, CONVERGENCE_TIMEOUT)
        .await?;
    let before_failover = wait_for_l1_state_with_timeout(
        cluster.node(1),
        "a co-signed batch is committed under the original settler",
        SETTLEMENT_LADDER_TIMEOUT,
        |state| state.last_committed_batch > 0,
    )
    .await?;

    // The settler is lost; promote validator 1 with its own operator identity.
    // Two standbys remain (2 and 3) — exactly the threshold.
    cluster.stop_validator(0).await?;
    let identity = SettlerIdentity::generate(0x83);
    authorize_and_fund(cluster.node(1), &identity).await?;
    cluster.stop_validator(1).await?;
    cluster
        .start_validator_with_config_overrides(1, |config| {
            config.batcher_config.enabled = true;
            identity.apply(config);
        })
        .await?;

    // The promoted settler re-collects signatures for everything it recreates
    // and keeps settling past the old settler's work — through to execution.
    wait_for_l1_state_with_timeout(
        cluster.node(1),
        "the promoted settler settles co-signed batches past the old settler's work",
        SETTLEMENT_LADDER_TIMEOUT,
        |state| state.last_executed_batch > before_failover.last_committed_batch,
    )
    .await?;

    // The old settler rejoins as a standby verifier.
    cluster
        .start_validator_with_config_overrides(0, |config| {
            config.batcher_config.enabled = false;
        })
        .await?;
    let tip = cluster.max_height().await?;
    cluster
        .wait_for_block_on_all(tip, CONVERGENCE_TIMEOUT)
        .await?;
    cluster.assert_block_hashes_agree(tip).await?;

    cluster.shutdown_all().await
}
