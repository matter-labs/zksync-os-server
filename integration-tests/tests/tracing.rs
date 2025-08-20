use alloy::primitives::{Bytes, U256};
use alloy::rpc::types::trace::geth::CallFrame;
use alloy::sol_types::{Revert, SolCall, SolError};
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
            error: Some("Reverted".to_string()),
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
            error: Some("Reverted".to_string()),
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
