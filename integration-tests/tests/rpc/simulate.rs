use alloy::consensus::{
    BlobTransactionSidecar, BlobTransactionSidecarVariant, SidecarBuilder, SimpleCoder,
    Transaction as _,
};
use alloy::eips::eip1559::Eip1559Estimation;
use alloy::network::primitives::BlockTransactions;
use alloy::network::{TransactionBuilder, TransactionBuilder4844};
use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::providers::utils::Eip1559Estimator;
use alloy::rpc::types::TransactionRequest;
use alloy::rpc::types::simulate::{SimBlock, SimulatePayload};
use alloy::rpc::types::state::{AccountOverride, StateOverridesBuilder};
use alloy::sol_types::{SolCall, SolEvent};
use zksync_os_integration_tests::contracts::EventEmitter::TestEvent;
use zksync_os_integration_tests::contracts::{Counter, EventEmitter};
use zksync_os_integration_tests::{CURRENT_TO_L1, NEXT_TO_GATEWAY, Tester, test_multisetup};

/// Simulate a single ETH transfer in one block and verify that gas was consumed.
#[test_multisetup([CURRENT_TO_L1])]
async fn simulate_eth_transfer_gas_used(tester: Tester) -> anyhow::Result<()> {
    let sender = tester.l2_wallet.default_signer().address();
    let recipient = alloy::primitives::address!("000000000000000000000000000000000000dead");
    let value = U256::from(1_000u64);

    let payload = SimulatePayload {
        block_state_calls: vec![
            SimBlock::default().call(
                TransactionRequest::default()
                    .from(sender)
                    .to(recipient)
                    .value(value),
            ),
        ],
        ..Default::default()
    };

    let results = tester.l2_provider.simulate(&payload).await?;

    assert_eq!(results.len(), 1, "expected one simulated block");
    let block = &results[0];
    assert_eq!(block.calls.len(), 1, "expected one call result");

    let call = &block.calls[0];
    assert!(call.status, "transfer should succeed");
    assert!(call.gas_used > 0, "gas used should be non-zero");

    Ok(())
}

/// Simulate three transactions in the same block and verify all succeed with non-zero gas.
#[test_multisetup([CURRENT_TO_L1])]
async fn simulate_multiple_txs_in_one_block(tester: Tester) -> anyhow::Result<()> {
    let sender = tester.l2_wallet.default_signer().address();
    let emitter = EventEmitter::deploy(tester.l2_provider.clone()).await?;
    let recipient = alloy::primitives::address!("000000000000000000000000000000000000dead");

    let payload = SimulatePayload {
        block_state_calls: vec![
            SimBlock::default()
                .call(emitter.emitEvent(U256::from(1)).into_transaction_request())
                .call(emitter.emitEvent(U256::from(2)).into_transaction_request())
                .call(
                    TransactionRequest::default()
                        .from(sender)
                        .to(recipient)
                        .value(U256::from(1u64)),
                ),
        ],
        ..Default::default()
    };

    let results = tester.l2_provider.simulate(&payload).await?;

    assert_eq!(results.len(), 1);
    let block = &results[0];
    assert_eq!(block.calls.len(), 3, "expected three call results");
    for (i, call) in block.calls.iter().enumerate() {
        assert!(call.status, "call {i} should succeed");
        assert!(call.gas_used > 0, "call {i} gas used should be non-zero");
    }

    // First two calls must have emitted exactly one TestEvent each, with numbers 1 and 2 in
    // that order. This proves that logs (and their ordering) are reported per-call.
    for (i, expected_number) in [1u64, 2u64].iter().enumerate() {
        let logs = &block.calls[i].logs;
        assert_eq!(logs.len(), 1, "call {i} should have one log");
        let decoded = TestEvent::decode_log(&logs[0].inner)
            .unwrap_or_else(|e| panic!("call {i} log is not a TestEvent: {e}"));
        assert_eq!(decoded.number, U256::from(*expected_number));
    }
    // The plain ETH transfer should not have produced any logs.
    assert!(
        block.calls[2].logs.is_empty(),
        "transfer should emit no logs"
    );

    Ok(())
}

/// Simulate three blocks in sequence and verify that state changes from earlier blocks carry over.
#[test_multisetup([CURRENT_TO_L1])]
async fn simulate_state_carries_across_blocks(tester: Tester) -> anyhow::Result<()> {
    // Deploy a Counter, increment by 7 in block 1, by 3 in block 2, then read the counter in
    // block 3 and confirm it equals 10. Without state carry-over, the read would return 0.
    let counter = Counter::deploy(tester.l2_provider.clone()).await?;

    let increment_call = counter.increment(U256::from(7)).into_transaction_request();
    let increment_call_2 = counter.increment(U256::from(3)).into_transaction_request();
    let read_call = counter.counter().into_transaction_request();

    let payload = SimulatePayload {
        block_state_calls: vec![
            SimBlock::default().call(increment_call),
            SimBlock::default().call(increment_call_2),
            SimBlock::default().call(read_call),
        ],
        ..Default::default()
    };

    let results = tester.l2_provider.simulate(&payload).await?;

    assert_eq!(results.len(), 3, "expected three simulated blocks");
    assert_eq!(results[0].calls.len(), 1);
    assert_eq!(results[1].calls.len(), 1);
    assert_eq!(results[2].calls.len(), 1);

    let first_call = &results[0].calls[0];
    let second_call = &results[1].calls[0];
    let read_result = &results[2].calls[0];

    assert!(first_call.status, "first increment should succeed");
    assert!(second_call.status, "second increment should succeed");
    assert!(read_result.status, "read should succeed");
    assert!(first_call.gas_used > 0);
    assert!(second_call.gas_used > 0);

    let observed = Counter::counterCall::abi_decode_returns(&read_result.return_data)?;
    assert_eq!(observed, U256::from(10), "counter should be 7 + 3 = 10");

    Ok(())
}

/// Simulate the transaction shape used by the settlement-layer sender through `eth_simulateV1`.
///
/// Direct-L1 commit transactions carry blob sidecars. Gateway commit transactions never
/// carry blobs, so the gateway case proves we do not need EIP-4844 support there.
#[test_multisetup([CURRENT_TO_L1, NEXT_TO_GATEWAY])]
async fn simulate_settlement_sender_tx_shape(tester: Tester) -> anyhow::Result<()> {
    let provider = tester.sl_provider();
    let settles_on_gateway = tester.gateway_eth_provider().is_some();
    let sender = if settles_on_gateway {
        tester
            .config()
            .gateway_sender_config
            .operator_commit_sk
            .as_ref()
            .expect("gateway commit signer should be configured")
            .address()
            .await?
    } else {
        tester.l1_wallet().default_signer().address()
    };
    let recipient = Address::with_last_byte(0x42);
    let nonce = provider.get_transaction_count(sender).pending().await?;
    let max_priority_fee_per_gas = provider.get_max_priority_fee_per_gas().await?;
    let fees = provider
        .estimate_eip1559_fees_with(Eip1559Estimator::new(|base_fee_per_gas, _| {
            Eip1559Estimation {
                max_fee_per_gas: base_fee_per_gas * 3 / 2,
                max_priority_fee_per_gas: 0,
            }
        }))
        .await?;
    let max_fee_per_gas = fees.max_fee_per_gas + max_priority_fee_per_gas;

    let mut request = TransactionRequest::default()
        .with_from(sender)
        .with_to(recipient)
        .with_max_fee_per_gas(max_fee_per_gas)
        .with_max_priority_fee_per_gas(max_priority_fee_per_gas)
        .with_nonce(nonce)
        .with_gas_limit(30_000_000);

    if !settles_on_gateway {
        let blob_sidecar: BlobTransactionSidecar =
            SidecarBuilder::<SimpleCoder>::from_slice(b"simulate-v1 blob sidecar")
                .build()
                .expect("test blob sidecar should be buildable");
        request.max_fee_per_blob_gas = Some(1_000_000_000u128);
        request.set_blob_sidecar(BlobTransactionSidecarVariant::Eip4844(blob_sidecar));
        request.transaction_type = Some(3);
    }

    let results = provider
        .simulate(&settlement_sender_simulate_payload(sender, request))
        .pending()
        .await?;
    assert_eq!(results.len(), 1, "expected one simulated L1 block");
    assert_eq!(results[0].calls.len(), 1, "expected one call result");
    let BlockTransactions::Full(transactions) = &results[0].inner.transactions else {
        panic!("expected full transaction response for simulated settlement tx");
    };
    assert_eq!(transactions.len(), 1, "expected one full transaction");
    if settles_on_gateway {
        assert!(
            transactions[0].blob_versioned_hashes().is_none(),
            "gateway settlement tx should not carry blobs",
        );
    } else {
        assert!(
            transactions[0]
                .blob_versioned_hashes()
                .is_some_and(|hashes| !hashes.is_empty()),
            "direct-L1 settlement tx should carry blob hashes",
        );
    }
    let call = &results[0].calls[0];
    assert!(call.status, "settlement sender simulation should succeed");
    assert!(
        call.gas_used > 0,
        "settlement sender simulation gas used should be non-zero"
    );

    Ok(())
}

fn settlement_sender_simulate_payload(
    sender: Address,
    request: TransactionRequest,
) -> SimulatePayload {
    let balance_override = StateOverridesBuilder::default()
        .append(
            sender,
            AccountOverride {
                balance: Some(U256::MAX),
                ..Default::default()
            },
        )
        .build();
    let mut sim_block = SimBlock::default().call(request);
    sim_block.state_overrides = Some(balance_override);

    SimulatePayload {
        block_state_calls: vec![sim_block],
        validation: false,
        return_full_transactions: true,
        ..Default::default()
    }
}
