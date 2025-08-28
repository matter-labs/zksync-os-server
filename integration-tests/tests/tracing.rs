use alloy::network::Ethereum;
use alloy::primitives::{Address, Bytes, U256};
use alloy::providers::PendingTransactionBuilder;
use alloy::rpc::types::trace::geth::CallFrame;
use alloy::sol_types::{Revert, SolCall, SolError};
use std::collections::HashMap;
use zksync_os_integration_tests::Tester;
use zksync_os_integration_tests::assert_traits::ReceiptAssert;
use zksync_os_integration_tests::contracts::{EventEmitter, TracingPrimary, TracingSecondary};
use zksync_os_integration_tests::dyn_wallet_provider::EthDynProvider;

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

async fn check_equivalency<
    Fut: Future<Output = anyhow::Result<PendingTransactionBuilder<Ethereum>>>,
>(
    name: &str,
    tester: &Tester,
    f: impl Fn(EthDynProvider) -> Fut,
) -> anyhow::Result<()> {
    tracing::info!(name, "checking trace equivalence");
    let l1_call_frame = f(tester.l1_provider.clone())
        .await?
        .expect_call_trace()
        .await?;
    let l2_call_frame = f(tester.l2_provider.clone())
        .await?
        .expect_call_trace()
        .await?;
    assert_eq_call_frames(&l1_call_frame, &l2_call_frame);
    tracing::info!(name, "successful trace equivalence");
    Ok(())
}

#[test_log::test(tokio::test)]
async fn call_trace_transaction_equivalency() -> anyhow::Result<()> {
    // Test that the node call traces are equivalent to L1 traces (produced by anvil).
    let tester = Tester::setup().await?;
    // Init data for `TracingSecondary`
    let secondary_data = U256::from(42);
    // Call value for `TracingPrimary::multiCalculate`
    let calculate_value = U256::from(24);
    let times = U256::from(10);

    check_equivalency("multi-subcall", &tester, |provider| async move {
        let secondary_contract = TracingSecondary::deploy(provider.clone(), secondary_data).await?;
        let primary_contract =
            TracingPrimary::deploy(provider, *secondary_contract.address()).await?;
        anyhow::Ok(
            primary_contract
                .multiCalculate(calculate_value, times)
                .send()
                .await?,
        )
    })
    .await?;

    check_equivalency("create", &tester, |provider| async move {
        Ok(EventEmitter::deploy_builder(provider).send().await?)
    })
    .await?;

    Ok(())
}

/// Asserts that two call frame trees are equivalent. Specifically excludes some fields that we do
/// not assert L1-L2 equivalency for (e.g., `gas`, `gasUsed`).
fn assert_eq_call_frames(l1_call_frame: &CallFrame, l2_call_frame: &CallFrame) {
    assert_eq_call_frames_internal(l1_call_frame, l2_call_frame, &mut HashMap::new());
}

fn assert_eq_call_frames_internal(
    l1_call_frame: &CallFrame,
    l2_call_frame: &CallFrame,
    address_mapping: &mut HashMap<Address, Address>,
) {
    let mut l1_call_frame = strip_call_frame(l1_call_frame);
    let l2_call_frame = strip_call_frame(l2_call_frame);
    if l1_call_frame.from != l2_call_frame.from {
        let mapped = address_mapping
            .entry(l1_call_frame.from)
            .or_insert(l2_call_frame.from);
        assert_eq!(
            mapped, &l2_call_frame.from,
            "L1 `from` address does not match mapped L2 `from` address"
        );
        l1_call_frame.from = *mapped;
    }
    if let Some(l1_to) = l1_call_frame.to
        && let Some(l2_to) = l2_call_frame.to
    {
        let mapped = address_mapping.entry(l1_to).or_insert(l2_to);
        assert_eq!(
            mapped, &l2_to,
            "L1 `to` address does not match mapped L2 `to` address"
        );
        l1_call_frame.to = Some(*mapped);
    }
    assert_eq!(l1_call_frame, l2_call_frame);
    assert_eq!(
        l1_call_frame.calls.len(),
        l2_call_frame.calls.len(),
        "call frames have different subcalls length"
    );
    for (l1, l2) in l1_call_frame.calls.iter().zip(l2_call_frame.calls.iter()) {
        assert_eq_call_frames_internal(l1, l2, address_mapping);
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
