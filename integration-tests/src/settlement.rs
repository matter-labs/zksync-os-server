//! Settler identities and committee-settlement test helpers, shared by the
//! settlement-failover and committee-batch-verification suites.

use crate::Tester;
use crate::assert_traits::ReceiptAssert;
use crate::l1_helpers::fetch_l1_state;
use crate::multi_node::MultiNodeTester;
use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, U256, utils::parse_ether};
use alloy::providers::ext::AnvilApi;
use alloy::providers::{PendingTransactionBuilder, Provider};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::k256::ecdsa::SigningKey;
use alloy::sol_types::SolCall;
use std::time::Duration;
use zksync_os_operator_signer::SignerConfig;

/// How long committee tests wait for cluster-wide effects. Sized for a loaded
/// machine (a full-suite run packs several committees concurrently, and CI
/// runners are slower still), not for the idle-machine happy path.
pub const CONVERGENCE_TIMEOUT: Duration = Duration::from_secs(120);

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
pub struct SettlerIdentity {
    pub commit: SigningKey,
    pub prove: SigningKey,
    pub execute: SigningKey,
}

impl SettlerIdentity {
    /// Deterministic per-test identities; `tag` keeps concurrent tests' keys apart.
    pub fn generate(tag: u8) -> Self {
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

    pub fn addresses(&self) -> [Address; 3] {
        [&self.commit, &self.prove, &self.execute]
            .map(|key| alloy::signers::local::LocalSigner::from(key.clone()).address())
    }

    /// Installs this identity as the node's operator keys.
    pub fn apply(&self, config: &mut zksync_os_server::config::Config) {
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
pub async fn authorize_and_fund(node: &Tester, identity: &SettlerIdentity) -> anyhow::Result<()> {
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
/// and each provider's nonce filler caches independently. It is pinned from the
/// *most-applied* view across the cluster, not from `via` alone: right after a
/// wallet-touching block lands (the funding deposit, an earlier transfer), a
/// validator that has not applied it yet reports a stale count — and a transfer
/// pinned to that count is below the account's true nonce, so every pool holds
/// or rejects it and no leader ever includes it. Whichever node confirmed the
/// previous receipt reports the true count, so the maximum is the truth.
///
/// The receipt is polled directly rather than through the provider's
/// block-subscription watcher: blocks arrive here by consensus commit, and a
/// watcher that misses one notification would report a timeout for a
/// transaction that landed long ago. On a real timeout, the error says whether
/// the node still holds the transaction — "stuck in the pool" and "lost
/// entirely" are different bugs.
pub async fn send_transfer(
    cluster: &MultiNodeTester,
    via: usize,
    recipient: Address,
) -> anyhow::Result<u64> {
    let node = cluster.node(via);
    let sender = node.l2_wallet.default_signer().address();
    let mut pending = 0;
    for (_, running) in cluster.running() {
        pending = pending.max(
            running
                .l2_provider
                .get_transaction_count(sender)
                .pending()
                .await?,
        );
    }
    let submitted = node
        .l2_provider
        .send_transaction(
            TransactionRequest::default()
                .with_to(recipient)
                .with_value(U256::from(1_000_000u64))
                .with_nonce(pending),
        )
        .await?;
    let hash = *submitted.tx_hash();

    let deadline = tokio::time::Instant::now() + CONVERGENCE_TIMEOUT;
    loop {
        if let Some(receipt) = node.l2_provider.get_transaction_receipt(hash).await? {
            anyhow::ensure!(
                receipt.status(),
                "transfer {hash} via validator {via} reverted",
            );
            return Ok(receipt
                .block_number
                .expect("included transactions have a block"));
        }
        if tokio::time::Instant::now() >= deadline {
            let still_known = node
                .l2_provider
                .get_transaction_by_hash(hash)
                .await?
                .is_some();
            anyhow::bail!(
                "transfer {hash} via validator {via} (nonce {pending}) was not included \
                 within {CONVERGENCE_TIMEOUT:?}; the node {}",
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
