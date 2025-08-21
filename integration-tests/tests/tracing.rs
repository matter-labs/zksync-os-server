use alloy::primitives::{Address, Bytes, U256};
use alloy::rpc::types::trace::geth::CallFrame;
use alloy::sol_types::{Revert, SolCall, SolError};
use std::collections::HashMap;
use zksync_os_integration_tests::Tester;
use zksync_os_integration_tests::assert_traits::ReceiptAssert;
use zksync_os_integration_tests::contracts::{TracingPrimary, TracingSecondary};

#[test_log::test(tokio::test)]
async fn call_trace_transaction() -> anyhow::Result<()> {
    // Test that the node can call trace an existing transaction. Manually asserts call trace output.
    let tester = Tester::setup().await?;
    let alice = tester.l2_wallet.default_signer().address();
    // Init data for `TracingSecondary`
    let secondary_data = U256::from(42);
    // Call value for `TracingPrimary::calculate`
    let calculate_value = U256::from(24);
    // Expected result for `TracingPrimary::calculate`
    let expected_value = secondary_data * calculate_value;

    let secondary_contract =
        TracingSecondary::deploy(tester.l2_provider.clone(), secondary_data).await?;
    let primary_contract =
        TracingPrimary::deploy(tester.l2_provider.clone(), *secondary_contract.address()).await?;

    let call_frame = primary_contract
        .calculate(calculate_value)
        .send()
        .await?
        .expect_call_trace()
        .await?;
    assert_eq!(
        call_frame,
        CallFrame {
            from: alice,
            to: Some(*primary_contract.address()),
            input: Bytes::from(
                TracingPrimary::calculateCall::SELECTOR
                    .into_iter()
                    .chain(calculate_value.to_be_bytes::<32>())
                    .collect::<Vec<u8>>()
            ),
            output: Some(Bytes::from(expected_value.to_be_bytes::<32>())),
            error: None,
            revert_reason: None,
            logs: vec![],
            value: Some(U256::ZERO),
            typ: "CALL".to_string(),
            // Below is not asserted
            gas: call_frame.gas,
            gas_used: call_frame.gas_used,
            calls: call_frame.calls.clone(),
        }
    );
    assert_eq!(call_frame.calls.len(), 1, "expected exactly 1 subcall");
    let subcall = &call_frame.calls[0];
    assert_eq!(
        subcall,
        &CallFrame {
            from: *primary_contract.address(),
            to: Some(*secondary_contract.address()),
            input: Bytes::from(
                TracingSecondary::multiplyCall::SELECTOR
                    .into_iter()
                    .chain(calculate_value.to_be_bytes::<32>())
                    .collect::<Vec<u8>>()
            ),
            output: Some(Bytes::from(expected_value.to_be_bytes::<32>())),
            error: None,
            revert_reason: None,
            logs: vec![],
            value: None,
            typ: "STATICCALL".to_string(),
            calls: vec![],
            // Below is not asserted
            gas: subcall.gas,
            gas_used: subcall.gas_used,
        }
    );

    let revert_call_frame = primary_contract
        .shouldRevert()
        // Set manual gas limit to avoid estimation failure
        .gas(1_000_000)
        .send()
        .await?
        .expect_call_trace()
        .await?;
    assert_eq!(
        revert_call_frame,
        CallFrame {
            from: alice,
            to: Some(*primary_contract.address()),
            input: Bytes::from(TracingPrimary::shouldRevertCall::SELECTOR),
            output: Some(Bytes::from(Revert::from("This should revert").abi_encode())),
            error: Some("execution reverted".to_string()),
            revert_reason: Some("This should revert".to_string()),
            logs: vec![],
            value: Some(U256::ZERO),
            typ: "CALL".to_string(),
            // Below is not asserted
            gas: revert_call_frame.gas,
            gas_used: revert_call_frame.gas_used,
            calls: revert_call_frame.calls.clone(),
        }
    );
    assert_eq!(
        revert_call_frame.calls.len(),
        1,
        "expected exactly 1 subcall"
    );
    let revert_subcall = &revert_call_frame.calls[0];
    assert_eq!(
        revert_subcall,
        &CallFrame {
            from: *primary_contract.address(),
            to: Some(*secondary_contract.address()),
            input: Bytes::from(TracingSecondary::shouldRevertCall::SELECTOR),
            output: Some(Bytes::from(Revert::from("This should revert").abi_encode())),
            error: Some("execution reverted".to_string()),
            revert_reason: Some("This should revert".to_string()),
            logs: vec![],
            value: None,
            typ: "STATICCALL".to_string(),
            calls: vec![],
            // Below is not asserted
            gas: revert_subcall.gas,
            gas_used: revert_subcall.gas_used,
        }
    );

    Ok(())
}

#[test_log::test(tokio::test)]
async fn call_trace_transaction_equivalency() -> anyhow::Result<()> {
    // Test that the node call traces are equivalent to L1 traces (produced by anvil).
    let tester = Tester::setup().await?;
    let l1_alice = tester.l1_wallet.default_signer().address();
    let l2_alice = tester.l2_wallet.default_signer().address();
    // Init data for `TracingSecondary`
    let secondary_data = U256::from(42);
    // Call value for `TracingPrimary::multiCalculate`
    let calculate_value = U256::from(24);
    let times = U256::from(10);

    let l1_secondary_contract =
        TracingSecondary::deploy(tester.l1_provider.clone(), secondary_data).await?;
    let l1_primary_contract =
        TracingPrimary::deploy(tester.l1_provider.clone(), *l1_secondary_contract.address())
            .await?;
    let mut l1_call_frame = l1_primary_contract
        .multiCalculate(calculate_value, times)
        .send()
        .await?
        .expect_call_trace()
        .await?;

    let l2_secondary_contract =
        TracingSecondary::deploy(tester.l2_provider.clone(), secondary_data).await?;
    let l2_primary_contract =
        TracingPrimary::deploy(tester.l2_provider.clone(), *l2_secondary_contract.address())
            .await?;
    let l2_call_frame = l2_primary_contract
        .multiCalculate(calculate_value, times)
        .send()
        .await?
        .expect_call_trace()
        .await?;
    convert_l1_call_frame_addresses(
        &mut l1_call_frame,
        &HashMap::from_iter([
            (l1_alice, l2_alice),
            (
                *l1_secondary_contract.address(),
                *l2_secondary_contract.address(),
            ),
            (
                *l1_primary_contract.address(),
                *l2_primary_contract.address(),
            ),
        ]),
    );
    assert_eq_call_frames(&l1_call_frame, &l2_call_frame);

    Ok(())
}

/// Asserts that two call frame trees are equivalent. Specifically excludes some fields that we do
/// not assert L1-L2 equivalency for (e.g., `gas`, `gasUsed`).
fn assert_eq_call_frames(l1_call_frame: &CallFrame, l2_call_frame: &CallFrame) {
    let l1_call_frame_stripped = strip_call_frame(l1_call_frame);
    let l2_call_frame_stripped = strip_call_frame(l2_call_frame);
    assert_eq!(l1_call_frame_stripped, l2_call_frame_stripped);
    assert_eq!(
        l1_call_frame.calls.len(),
        l2_call_frame.calls.len(),
        "call frames have different subcalls length"
    );
    for (l1, l2) in l1_call_frame.calls.iter().zip(l2_call_frame.calls.iter()) {
        assert_eq_call_frames(l1, l2);
    }
}

/// Strips call frame fields that we do not assert L1-L2 equivalency for.
fn strip_call_frame(call_frame: &CallFrame) -> CallFrame {
    let mut call_frame = call_frame.clone();
    call_frame.gas = U256::ZERO;
    call_frame.gas_used = U256::ZERO;
    call_frame.calls = vec![];
    call_frame
}

/// Converts `from`/`to` addresses in L1 call frame tree to the corresponding L2 addresses.
fn convert_l1_call_frame_addresses(
    call_frame: &mut CallFrame,
    address_mapping: &HashMap<Address, Address>,
) {
    call_frame.from = address_mapping
        .get(&call_frame.from)
        .copied()
        .unwrap_or(call_frame.from);
    call_frame.to = call_frame
        .to
        .map(|addr| address_mapping.get(&addr).copied().unwrap_or(addr));
    for call in &mut call_frame.calls {
        convert_l1_call_frame_addresses(call, address_mapping);
    }
}
