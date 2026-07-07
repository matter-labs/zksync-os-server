//! Settlement continuity on a committee: exactly one validator (the settler) runs
//! the batcher, every other validator keeps a full batcher configuration staged
//! with `batcher.enabled = false`, and failover is promotion by restart — flip the
//! flag on a standby with its *own* pre-authorized operator keys.
//!
//! No lease or handover protocol exists, deliberately: L1 itself is the mutual
//! exclusion. Two settlers signing with different operator identities race on
//! `commitBatchesSharedBridge`; L1 serializes them, the loser's transaction
//! reverts, and the loser dies loudly (`validate_tx_receipt` → critical pipeline
//! task). Its restart reconciles against L1 discovery like any batcher restart.
//! These tests drill both halves: the clean promotion, and the collision.

use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, U256, utils::parse_ether};
use alloy::providers::ext::AnvilApi;
use alloy::providers::{PendingTransactionBuilder, Provider};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::k256::ecdsa::SigningKey;
use alloy::sol_types::SolCall;
use std::time::Duration;
use zksync_os_integration_tests::Tester;
use zksync_os_integration_tests::assert_traits::ReceiptAssert;
use zksync_os_integration_tests::l1_helpers::{fetch_l1_state, wait_for_l1_state};
use zksync_os_integration_tests::multi_node::MultiNodeTester;
use zksync_os_operator_signer::SignerConfig;

const CONVERGENCE_TIMEOUT: Duration = Duration::from_secs(60);

alloy::sol! {
    // The ValidatorTimelock's operator-management surface (era-contracts v31):
    // per-chain roles (precommit/commit/revert/prove/execute), administered by
    // the chain's admin. `addValidatorForChainId` grants the full operator set.
    #[sol(rpc)]
    interface ITimelockValidators {
        function addValidatorForChainId(uint256 _chainId, address _validator) external;
        function hasRoleForChainId(uint256 _chainId, bytes32 _role, address _address) external view returns (bool);
        function COMMITTER_ROLE() external view returns (bytes32);
    }
}

/// One settler's on-chain identity: three distinct signers (commit / prove /
/// execute), mirroring production where every validator holds its own operator
/// keys and key access never moves during a failover.
struct SettlerIdentity {
    commit: SigningKey,
    prove: SigningKey,
    execute: SigningKey,
}

impl SettlerIdentity {
    /// Deterministic per-test identities; `tag` keeps concurrent tests' keys apart.
    fn generate(tag: u8) -> Self {
        let key = |suffix: u8| {
            let mut seed = [tag; 32];
            seed[31] = suffix;
            SigningKey::from_slice(&seed).expect("static test key")
        };
        Self {
            commit: key(1),
            prove: key(2),
            execute: key(3),
        }
    }

    fn addresses(&self) -> [Address; 3] {
        [&self.commit, &self.prove, &self.execute]
            .map(|key| alloy::signers::local::LocalSigner::from(key.clone()).address())
    }

    /// Installs this identity as the node's operator keys.
    fn apply(&self, config: &mut zksync_os_server::config::Config) {
        config.l1_sender_config.operator_commit_sk = Some(SignerConfig::Local(self.commit.clone()));
        config.l1_sender_config.operator_prove_sk = Some(SignerConfig::Local(self.prove.clone()));
        config.l1_sender_config.operator_execute_sk =
            Some(SignerConfig::Local(self.execute.clone()));
    }
}

/// The out-of-band half of standing up a standby settler, compressed to anvil
/// speed: authorize the identity on the ValidatorTimelock's per-chain operator
/// roles (impersonating the chain admin — on a real chain this is a governance
/// action) and fund its addresses.
async fn authorize_and_fund(node: &Tester, identity: &SettlerIdentity) -> anyhow::Result<()> {
    let l1_state = fetch_l1_state(node).await?;
    let timelock = l1_state.validator_timelock_sl;
    let chain_admin = l1_state.diamond_proxy_l1.get_admin().await?;
    let chain_id = node.l2_provider.get_chain_id().await?;
    let provider = node.l1_provider();

    provider.anvil_impersonate_account(chain_admin).await?;
    provider
        .anvil_set_balance(chain_admin, parse_ether("1")?)
        .await?;
    for operator in identity.addresses() {
        provider
            .anvil_set_balance(operator, parse_ether("100")?)
            .await?;
        let call = ITimelockValidators::addValidatorForChainIdCall {
            _chainId: U256::from(chain_id),
            _validator: operator,
        };
        let tx = TransactionRequest::default()
            .from(chain_admin)
            .to(timelock)
            .with_input(call.abi_encode());
        // Surface a real error message before the impersonated send (which
        // reports failures poorly) — same idiom as the upgrade tester.
        let _ = provider.estimate_gas(tx.clone()).await?;
        let hash = provider.anvil_send_impersonated_transaction(tx).await?;
        PendingTransactionBuilder::new(provider.root().clone(), hash)
            .expect_successful_receipt()
            .await?;
    }

    let registry = ITimelockValidators::new(timelock, provider.clone());
    let committer_role = registry.COMMITTER_ROLE().call().await?;
    anyhow::ensure!(
        registry
            .hasRoleForChainId(
                U256::from(chain_id),
                committer_role,
                identity.addresses()[0]
            )
            .call()
            .await?,
        "the commit operator must hold the committer role after authorization",
    );
    Ok(())
}

/// Submits a transfer through the validator at `via` and waits for inclusion.
/// The nonce is pinned explicitly: one wallet acts through many providers here,
/// and each provider's nonce filler caches independently.
async fn send_transfer(
    cluster: &MultiNodeTester,
    via: usize,
    recipient: Address,
) -> anyhow::Result<u64> {
    use anyhow::Context as _;
    let sender = cluster.node(via).l2_wallet.default_signer().address();
    let pending = cluster
        .node(via)
        .l2_provider
        .get_transaction_count(sender)
        .pending()
        .await?;
    let receipt = tokio::time::timeout(CONVERGENCE_TIMEOUT, async {
        cluster
            .node(via)
            .l2_provider
            .send_transaction(
                TransactionRequest::default()
                    .with_to(recipient)
                    .with_value(U256::from(1_000_000u64))
                    .with_nonce(pending),
            )
            .await?
            .expect_successful_receipt()
            .await
    })
    .await
    .map_err(|_| anyhow::anyhow!("transaction was not included within {CONVERGENCE_TIMEOUT:?}"))?
    .with_context(|| format!("transfer via validator {via}"))?;
    Ok(receipt
        .block_number
        .expect("included transactions have a block"))
}

/// The failover drill: the settler dies, a standby is promoted by restart with its
/// own operator identity, recreates the committed batches from L1 discovery,
/// re-proves what the dead settler had in flight, and settlement resumes gapless —
/// while sequencing never stops. The old settler then rejoins as a standby (its
/// config demoted — the easy-to-forget half of a failover).
#[test_log::test(tokio::test)]
async fn settlement_fails_over_to_a_promoted_standby() -> anyhow::Result<()> {
    let mut cluster = MultiNodeTester::start(4).await?;

    // Settlement is live under the original settler (validator 0), with work in
    // flight: a batch committed but not yet executed is exactly what the promoted
    // standby must pick up and re-drive.
    let included_at = send_transfer(&cluster, 1, Address::repeat_byte(0x51)).await?;
    cluster
        .wait_for_block_on_all(included_at, CONVERGENCE_TIMEOUT)
        .await?;
    let before_failover = wait_for_l1_state(
        cluster.node(1),
        "a batch committed but not yet executed on L1",
        |state| state.last_committed_batch > state.last_executed_batch,
    )
    .await?;

    // The settler is lost. Sequencing rides on (3-of-4 quorum) while settlement
    // freezes — both commit and execute senders lived on the dead node.
    cluster.stop_validator(0).await?;
    let while_down = send_transfer(&cluster, 1, Address::repeat_byte(0x52)).await?;
    cluster
        .wait_for_block_on_all(while_down, CONVERGENCE_TIMEOUT)
        .await?;
    let frozen = fetch_l1_state(cluster.node(1)).await?;

    // Promote validator 1: authorize its own operator identity (the on-chain
    // governance half), then flip `batcher.enabled` and restart — the entire
    // in-band failover procedure.
    let identity = SettlerIdentity::generate(0x51);
    authorize_and_fund(cluster.node(1), &identity).await?;
    let promotion_started = std::time::Instant::now();
    cluster.stop_validator(1).await?;
    cluster
        .start_validator_with_config_overrides(1, |config| {
            config.batcher_config.enabled = true;
            identity.apply(config);
        })
        .await?;

    // The promoted settler recreates committed batches from L1, re-proves the
    // in-flight work with its own prover pipeline, and settles *past* everything
    // the dead settler ever did — commit, prove, and execute all resumed.
    wait_for_l1_state(
        cluster.node(1),
        "the promoted settler resumes settlement past the old settler's work",
        |state| {
            state.last_committed_batch > frozen.last_committed_batch
                && state.last_executed_batch > before_failover.last_committed_batch
        },
    )
    .await?;
    tracing::info!(
        recovery_time = ?promotion_started.elapsed(),
        "settlement resumed under the promoted settler",
    );

    // The old settler rejoins as a standby: same node, same state, batcher off —
    // restoring the exactly-one-settler invariant is part of the failover, not an
    // afterthought.
    cluster
        .start_validator_with_config_overrides(0, |config| {
            config.batcher_config.enabled = false;
        })
        .await?;
    // Let the rejoined node catch up before submitting through it: a replaying
    // node serves a stale account state, so a transfer built against its nonce
    // view would be unincludable.
    let tip = cluster.max_height().await?;
    cluster
        .wait_for_block_on_all(tip, CONVERGENCE_TIMEOUT)
        .await?;
    let after_rejoin = send_transfer(&cluster, 0, Address::repeat_byte(0x53)).await?;
    cluster
        .wait_for_block_on_all(after_rejoin, CONVERGENCE_TIMEOUT)
        .await?;
    cluster.assert_block_hashes_agree(after_rejoin).await?;

    // Settlement keeps moving with the full committee back.
    let at_rejoin = fetch_l1_state(cluster.node(1)).await?;
    wait_for_l1_state(
        cluster.node(1),
        "settlement continues after the old settler rejoined as standby",
        |state| state.last_committed_batch > at_rejoin.last_committed_batch,
    )
    .await?;

    cluster.shutdown_all().await
}

/// The split-brain drill: two batcher-enabled validators, each with its own
/// operator identity, race on L1. L1 serializes; the loser's command reverts and
/// the loser dies loudly with an error naming the likely cause — while sequencing
/// never notices, the survivor keeps settling, and the loser rejoins cleanly as a
/// standby once its config is fixed.
#[test_log::test(tokio::test)]
async fn a_colliding_second_settler_dies_loudly_and_the_committee_recovers() -> anyhow::Result<()> {
    let mut cluster = MultiNodeTester::start(4).await?;

    let included_at = send_transfer(&cluster, 3, Address::repeat_byte(0x61)).await?;
    cluster
        .wait_for_block_on_all(included_at, CONVERGENCE_TIMEOUT)
        .await?;
    let before_collision = wait_for_l1_state(
        cluster.node(3),
        "settlement is live under the original settler",
        |state| state.last_committed_batch > 0,
    )
    .await?;

    // The misconfiguration under drill: validator 1 comes up batcher-enabled
    // with its own (authorized, funded) identity while validator 0 still
    // settles. The L1 mutual exclusion has two teeth, and which one bites is a
    // race: usually the second settler's commit watcher sees a foreign batch
    // land moments after its own startup and executes it *at birth* (the launch
    // itself fails); if it survives birth, the two race until one loses a
    // commit on L1 and dies.
    use anyhow::Context as _;
    let identity = SettlerIdentity::generate(0x61);
    authorize_and_fund(cluster.node(3), &identity).await?;
    cluster.stop_validator(1).await?;
    let launch = cluster
        .start_validator_with_config_overrides(1, |config| {
            config.batcher_config.enabled = true;
            identity.apply(config);
        })
        .await;

    let loser = match launch {
        // Killed at birth: the guard refused to run a second settler at all.
        // Validator 1's harness slot is spent for this run; the drill's
        // remaining claims are the survivor's.
        Err(launch_error) => {
            tracing::info!(
                %launch_error,
                "the second settler was killed at birth by the unexpected-commit guard",
            );
            None
        }
        // Both are up: keep batches coming until L1 serializes a collision. The
        // senders await each command's receipt before the next, so exactly one
        // settler loses one race and dies; the other never observes a failure.
        Ok(()) => {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
            let mut loser = None;
            let mut salt = 0x70u8;
            while loser.is_none() {
                anyhow::ensure!(
                    tokio::time::Instant::now() < deadline,
                    "no settler died within 120s: the L1 mutual exclusion never fired",
                );
                // Traffic through a node that is never a settler; failures are
                // irrelevant (a dying settler may briefly wobble the committee).
                let _ = send_transfer(&cluster, 3, Address::repeat_byte(salt)).await;
                salt = salt.wrapping_add(1);
                loser = [0usize, 1]
                    .into_iter()
                    .find(|i| cluster.node(*i).has_crashed());
            }
            Some(loser.expect("loop exits only with a loser"))
        }
    };

    if let Some(loser) = loser {
        // The loser died for the documented reason, naming the remedy — and the
        // fix is the standby demotion: exactly the runbook's recovery step.
        let survivor = 1 - loser;
        let error = cluster
            .node_mut(loser)
            .wait_for_fatal_error_with_timeout(Duration::from_secs(5))
            .await?
            .to_string();
        anyhow::ensure!(
            error.contains("another settler"),
            "the collision death must name the likely cause, got: {error}",
        );
        anyhow::ensure!(
            !cluster.node(survivor).has_crashed(),
            "both settlers died — L1 should have let exactly one win each race",
        );
        cluster
            .stop_validator(loser)
            .await
            .context("stopping the crashed loser")?;
        cluster
            .start_validator_with_config_overrides(loser, |config| {
                config.batcher_config.enabled = false;
            })
            .await
            .context("restarting the loser as a standby")?;
    }

    // In either shape: sequencing never depended on the loser, the committee
    // still agrees, and the surviving settler keeps settling.
    let after_recovery = send_transfer(&cluster, 3, Address::repeat_byte(0x69)).await?;
    cluster
        .wait_for_block_on_all(after_recovery, CONVERGENCE_TIMEOUT)
        .await
        .context("converging after the collision resolved")?;
    cluster
        .assert_block_hashes_agree(after_recovery)
        .await
        .context("hash agreement after recovery")?;
    wait_for_l1_state(
        cluster.node(3),
        "the surviving settler keeps settling",
        |state| state.last_committed_batch > before_collision.last_committed_batch,
    )
    .await?;

    cluster.shutdown_all().await
}
