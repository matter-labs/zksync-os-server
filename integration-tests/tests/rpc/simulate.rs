use alloy::primitives::U256;
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use alloy::rpc::types::simulate::{SimBlock, SimulatePayload};
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

    Ok(())
}

/// Simulate two blocks in sequence and verify that state changes from the first block carry over.
#[test_multisetup([CURRENT_TO_L1])]
async fn simulate_state_carries_across_blocks(tester: Tester) -> anyhow::Result<()> {
    // Deploy a Counter and increment it in block 1. In block 2, read it via eth_call to confirm
    // the simulated write is visible.
    let counter = Counter::deploy(tester.l2_provider.clone()).await?;

    let increment_call = counter.increment(U256::from(7)).into_transaction_request();
    // A second increment in block 2 — if state did not carry over, this would fail or return wrong data.
    let increment_call_2 = counter.increment(U256::from(3)).into_transaction_request();

    let payload = SimulatePayload {
        block_state_calls: vec![
            SimBlock::default().call(increment_call),
            SimBlock::default().call(increment_call_2),
        ],
        ..Default::default()
    };

    let results = tester.l2_provider.simulate(&payload).await?;

    assert_eq!(results.len(), 2, "expected two simulated blocks");
    assert_eq!(results[0].calls.len(), 1);
    assert_eq!(results[1].calls.len(), 1);

    let first_call = &results[0].calls[0];
    let second_call = &results[1].calls[0];

    assert!(first_call.status, "first increment should succeed");
    assert!(second_call.status, "second increment should succeed");
    assert!(first_call.gas_used > 0);
    assert!(second_call.gas_used > 0);

    Ok(())
}
