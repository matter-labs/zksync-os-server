use alloy::consensus::Transaction as ConsensusTransaction;
use alloy::eips::BlockNumberOrTag;
use alloy::network::TransactionBuilder;
use alloy::network::TransactionResponse;
use alloy::network::primitives::BlockTransactions;
use alloy::primitives::{Address, B256, U256};
use alloy::providers::Provider;
use alloy::rpc::types::{Filter, TransactionRequest};
use alloy::sol_types::SolEvent;
use smart_config::EtherAmount;
use zksync_os_contract_interface::IExecutor::{BlockCommit, BlockExecution};
use zksync_os_contract_interface::l1_discovery::L1State;
use zksync_os_integration_tests::assert_traits::{DEFAULT_TIMEOUT, ReceiptAssert};
use zksync_os_integration_tests::provider::{ZksyncApi, ZksyncTestingProvider};
use zksync_os_integration_tests::{CURRENT_TO_L1, TestEnvironment, Tester, test_multisetup};

// Distinct prime-ish wei amounts so a mismatch in the test points unambiguously
// at the wrong field. All large enough to stay above anvil's base fee estimates.
const TEST_MAX_FEE_PER_GAS_WEI: u128 = 137_000_000_017;
const TEST_MAX_PRIORITY_FEE_PER_GAS_WEI: u128 = 3_000_000_011;
const TEST_MAX_FEE_PER_BLOB_GAS_WEI: u128 = 7_000_000_019;

#[test_multisetup([CURRENT_TO_L1])]
#[test_runtime(flavor = "multi_thread")]
async fn l1_sender_uses_configured_static_fee_caps(env: TestEnvironment) -> anyhow::Result<()> {
    let mut config = env.default_config().await?;
    config.l1_sender_config.max_fee_per_gas = EtherAmount(TEST_MAX_FEE_PER_GAS_WEI);
    config.l1_sender_config.max_priority_fee_per_gas =
        EtherAmount(TEST_MAX_PRIORITY_FEE_PER_GAS_WEI);
    config.l1_sender_config.max_fee_per_blob_gas = EtherAmount(TEST_MAX_FEE_PER_BLOB_GAS_WEI);

    let tester = env.launch(config).await?;

    // Trigger a batch by sending an L2 transaction and waiting for it to be
    // executed (i.e. finalized on L1). Once finalized, commit and execute
    // operators have both produced their L1 transactions.
    let receipt = tester
        .l2_provider
        .send_transaction(
            TransactionRequest::default()
                .with_to(Address::random())
                .with_value(U256::from(1u64)),
        )
        .await?
        .expect_successful_receipt()
        .await?;
    let l2_block = receipt.block_number.expect("receipt has block number");
    tester
        .l2_zk_provider
        .wait_finalized_with_timeout(l2_block, DEFAULT_TIMEOUT)
        .await?;

    // Resolve the diamond proxy address so we can filter its events for the
    // exact L1 tx hashes of the commit and execute transactions.
    let chain_id = tester.l2_provider.get_chain_id().await?;
    let bridgehub_address = tester.l2_zk_provider.get_bridgehub_contract().await?;
    let l1_state = L1State::fetch(
        tester.l1_provider().clone().erased(),
        tester.gateway_eth_provider(),
        bridgehub_address,
        chain_id,
    )
    .await?;
    let diamond_proxy_address = *l1_state.diamond_proxy_sl.address();

    let (commit_tx_hash, commit_block) =
        find_event_tx(&tester, diamond_proxy_address, BlockCommit::SIGNATURE_HASH).await?;
    let (execute_tx_hash, execute_block) = find_event_tx(
        &tester,
        diamond_proxy_address,
        BlockExecution::SIGNATURE_HASH,
    )
    .await?;

    assert_static_fees(&tester, commit_tx_hash, "commit", true).await?;
    assert_static_fees(&tester, execute_tx_hash, "execute", false).await?;

    // Prove submission emits no event, so we locate its tx by scanning the
    // small L1 block range between commit and execute for a transaction from
    // the prove operator. There must be exactly one — the prove tx for our
    // single batch.
    let prove_operator_address = tester
        .config()
        .l1_sender_config
        .operator_prove_sk
        .as_ref()
        .expect("prove operator signer must be configured")
        .address()
        .await?;
    let prove_tx_hash =
        find_tx_from_in_range(&tester, prove_operator_address, commit_block, execute_block).await?;
    assert_static_fees(&tester, prove_tx_hash, "prove", false).await?;

    Ok(())
}

/// Fetches the first log of the given event from the diamond proxy and returns
/// the L1 tx hash that produced it together with its L1 block number.
async fn find_event_tx(
    tester: &Tester,
    diamond_proxy: Address,
    event_signature: B256,
) -> anyhow::Result<(B256, u64)> {
    let filter = Filter::new()
        .from_block(0u64)
        .event_signature(event_signature)
        .address(diamond_proxy);
    let logs = tester.l1_provider().get_logs(&filter).await?;
    let log = logs
        .first()
        .ok_or_else(|| anyhow::anyhow!("no log matched event {event_signature:?}"))?;
    let tx_hash = log
        .transaction_hash
        .ok_or_else(|| anyhow::anyhow!("indexed log without tx hash"))?;
    let block_number = log
        .block_number
        .ok_or_else(|| anyhow::anyhow!("indexed log without block number"))?;
    Ok((tx_hash, block_number))
}

/// Scans an L1 block range for a single transaction signed by `from`. Used to
/// locate the prove transaction, which emits no event we can filter on.
async fn find_tx_from_in_range(
    tester: &Tester,
    from: Address,
    from_block: u64,
    to_block: u64,
) -> anyhow::Result<B256> {
    let mut matches = Vec::new();
    for n in from_block..=to_block {
        let block = tester
            .l1_provider()
            .get_block_by_number(BlockNumberOrTag::Number(n))
            .full()
            .await?
            .ok_or_else(|| anyhow::anyhow!("L1 block {n} missing"))?;
        let BlockTransactions::Full(txs) = block.transactions else {
            anyhow::bail!("expected full transactions for block {n}");
        };
        for tx in txs {
            if tx.from() == from {
                matches.push(*tx.inner.tx_hash());
            }
        }
    }
    match matches.as_slice() {
        [tx] => Ok(*tx),
        [] => anyhow::bail!("no tx from {from:?} found in blocks {from_block}..={to_block}"),
        more => anyhow::bail!(
            "expected exactly one tx from {from:?} in blocks {from_block}..={to_block}, found {}: {more:?}",
            more.len(),
        ),
    }
}

/// Asserts that the given L1 transaction carries the configured static fee caps.
async fn assert_static_fees(
    tester: &Tester,
    tx_hash: B256,
    label: &str,
    expect_blob: bool,
) -> anyhow::Result<()> {
    let tx = tester
        .l1_provider()
        .get_transaction_by_hash(tx_hash)
        .await?
        .ok_or_else(|| anyhow::anyhow!("{label} tx {tx_hash:?} not found on L1"))?;

    assert_eq!(
        ConsensusTransaction::max_fee_per_gas(&tx),
        TEST_MAX_FEE_PER_GAS_WEI,
        "{label} tx {tx_hash:?}: max_fee_per_gas must equal configured cap",
    );
    // Priority fee is capped by — but may be less than — the configured value.
    let priority = ConsensusTransaction::max_priority_fee_per_gas(&tx)
        .expect("EIP-1559 / 4844 tx always carries max_priority_fee_per_gas");
    assert!(
        priority <= TEST_MAX_PRIORITY_FEE_PER_GAS_WEI,
        "{label} tx {tx_hash:?}: priority fee {priority} exceeds configured cap {}",
        TEST_MAX_PRIORITY_FEE_PER_GAS_WEI,
    );
    if expect_blob {
        let blob_fee = ConsensusTransaction::max_fee_per_blob_gas(&tx).ok_or_else(|| {
            anyhow::anyhow!("{label} tx {tx_hash:?} expected to carry blob fee but did not")
        })?;
        assert_eq!(
            blob_fee, TEST_MAX_FEE_PER_BLOB_GAS_WEI,
            "{label} tx {tx_hash:?}: max_fee_per_blob_gas must equal configured cap",
        );
    }
    Ok(())
}
