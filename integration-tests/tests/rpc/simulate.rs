use alloy::primitives::U256;
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use alloy::rpc::types::simulate::{SimBlock, SimulatePayload};
use alloy::sol_types::{SolCall, SolEvent};
use zksync_os_integration_tests::contracts::EventEmitter::TestEvent;
use zksync_os_integration_tests::contracts::{Counter, EventEmitter};
use zksync_os_integration_tests::{CURRENT_TO_L1, Tester, test_multisetup};

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
