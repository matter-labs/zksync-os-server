use alloy::eips::BlockId;
use alloy::network::{Ethereum, TransactionBuilder};
use alloy::primitives::{Address, Bytes, U256};
use alloy::providers::PendingTransactionBuilder;
use alloy::providers::ext::DebugApi;
use alloy::rpc::types::TransactionRequest;
use alloy::rpc::types::trace::geth::{
    CallConfig, CallFrame, GethDebugTracerType, GethDebugTracingCallOptions,
    GethDebugTracingOptions, GethTrace,
};
use alloy::sol_types::{Revert, SolCall, SolError};
use std::collections::HashMap;
use zksync_os_integration_tests::assert_traits::{ReceiptAssert, ReceiptsAssert};
use zksync_os_integration_tests::contracts::{EventEmitter, TracingPrimary, TracingSecondary};
use zksync_os_integration_tests::dyn_wallet_provider::EthDynProvider;
use zksync_os_integration_tests::{CURRENT_TO_L1, Tester, test_multisetup};

fn check_call_frame(
    call_frame: CallFrame,
    alice: Address,
    calculate_value: U256,
    expected_value: U256,
    primary_contract: Address,
    secondary_contract: Address,
) {
    assert_eq!(
        call_frame,
        CallFrame {
            from: alice,
            to: Some(primary_contract),
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
            from: primary_contract,
            to: Some(secondary_contract),
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
}

#[test_multisetup([CURRENT_TO_L1])]
async fn call_trace_transaction(tester: Tester) -> anyhow::Result<()> {
    // Test that the node can call trace an existing transaction. Manually asserts call trace output.
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
    check_call_frame(
        call_frame,
        alice,
        calculate_value,
        expected_value,
        *primary_contract.address(),
        *secondary_contract.address(),
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

async fn check_tx_equivalency<
    Fut: Future<Output = anyhow::Result<PendingTransactionBuilder<Ethereum>>>,
>(
    name: &str,
    tester: &Tester,
    f: impl Fn(EthDynProvider) -> Fut,
) -> anyhow::Result<()> {
    tracing::info!(name, "checking trace equivalence");
    let l1_call_frame = f(tester.l1_provider().clone())
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

async fn check_call_equivalency<Fut: Future<Output = anyhow::Result<TransactionRequest>>>(
    name: &str,
    tester: &Tester,
    f: impl Fn(EthDynProvider) -> Fut,
) -> anyhow::Result<()> {
    tracing::info!(name, "checking trace equivalence");
    let l1_tx_request = f(tester.l1_provider().clone()).await?;
    let l1_call_frame = tester
        .l1_provider()
        .debug_trace_call(
            l1_tx_request,
            BlockId::latest(),
            GethDebugTracingOptions::call_tracer(CallConfig::default()).into(),
        )
        .await?
        .try_into_call_frame()
        .expect("not a call frame");
    let l2_tx_request = f(tester.l2_provider.clone()).await?;
    let l2_call_frame = tester
        .l2_provider
        .debug_trace_call(
            l2_tx_request,
            BlockId::latest(),
            GethDebugTracingOptions::call_tracer(CallConfig::default()).into(),
        )
        .await?
        .try_into_call_frame()
        .expect("not a call frame");
    assert_eq_call_frames(&l1_call_frame, &l2_call_frame);
    tracing::info!(name, "successful trace equivalence");
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

#[test_multisetup([CURRENT_TO_L1])]
async fn call_trace_transaction_equivalency(tester: Tester) -> anyhow::Result<()> {
    // Test that the node call traces are equivalent to L1 traces (produced by anvil).
    // Init data for `TracingSecondary`
    let secondary_data = U256::from(42);
    // Call value for `TracingPrimary::multiCalculate`
    let calculate_value = U256::from(24);
    let times = U256::from(10);

    check_tx_equivalency("multi-subcall", &tester, |provider| async move {
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

    check_tx_equivalency("create", &tester, |provider| async move {
        Ok(EventEmitter::deploy_builder(provider).send().await?)
    })
    .await?;

    Ok(())
}

#[test_multisetup([CURRENT_TO_L1])]
async fn call_trace_equivalency(tester: Tester) -> anyhow::Result<()> {
    // Test that the `debug_traceCall` output is equivalent to L1 output (as produced by anvil).
    // Init data for `TracingSecondary`
    let secondary_data = U256::from(42);
    // Call value for `TracingPrimary::multiCalculate`
    let calculate_value = U256::from(24);
    let times = U256::from(10);

    check_call_equivalency("multi-subcall", &tester, |provider| async move {
        let secondary_contract = TracingSecondary::deploy(provider.clone(), secondary_data).await?;
        let primary_contract =
            TracingPrimary::deploy(provider, *secondary_contract.address()).await?;
        anyhow::Ok(
            primary_contract
                .multiCalculate(calculate_value, times)
                .into_transaction_request(),
        )
    })
    .await?;

    check_call_equivalency("create", &tester, |provider| async move {
        Ok(EventEmitter::deploy_builder(provider).into_transaction_request())
    })
    .await?;

    Ok(())
}

#[test_multisetup([CURRENT_TO_L1])]
async fn call_trace_block(tester: Tester) -> anyhow::Result<()> {
    // Test that the node call traces are equivalent to L1 traces (produced by anvil).
    let alice = tester.l2_wallet.default_signer().address();
    // Init data for `TracingSecondary`
    let secondary_data = U256::from(42);
    // Call values for `TracingPrimary::calculate`
    let calculate_value0 = U256::from(24);
    let calculate_value1 = U256::from(25);
    // Expected results for `TracingPrimary::calculate`
    let expected_value0 = secondary_data * calculate_value0;
    let expected_value1 = secondary_data * calculate_value1;

    let secondary_contract =
        TracingSecondary::deploy(tester.l2_provider.clone(), secondary_data).await?;
    let primary_contract =
        TracingPrimary::deploy(tester.l2_provider.clone(), *secondary_contract.address()).await?;

    loop {
        let tx0 = primary_contract.calculate(calculate_value0).send().await?;
        let tx1 = primary_contract.calculate(calculate_value1).send().await?;

        let receipts = vec![tx0, tx1].expect_successful_receipts().await?;
        if receipts[0].block_number.unwrap() != receipts[1].block_number.unwrap() {
            tracing::info!("transactions got mined in different blocks, retrying");
            continue;
        }
        let block_number = receipts[0].block_number.unwrap();
        let traces = tester
            .l2_provider
            .debug_trace_block_by_number(
                block_number.into(),
                GethDebugTracingOptions::call_tracer(CallConfig::default()),
            )
            .await?;

        let call_frame0 = traces
            .iter()
            .find_map(|trace| {
                if trace.tx_hash() == Some(receipts[0].transaction_hash) {
                    Some(
                        trace
                            .success()
                            .unwrap()
                            .clone()
                            .try_into_call_frame()
                            .unwrap(),
                    )
                } else {
                    None
                }
            })
            .expect("block traces did not contain trace for tx0");
        check_call_frame(
            call_frame0,
            alice,
            calculate_value0,
            expected_value0,
            *primary_contract.address(),
            *secondary_contract.address(),
        );
        let call_frame1 = traces
            .iter()
            .find_map(|trace| {
                if trace.tx_hash() == Some(receipts[1].transaction_hash) {
                    Some(
                        trace
                            .success()
                            .unwrap()
                            .clone()
                            .try_into_call_frame()
                            .unwrap(),
                    )
                } else {
                    None
                }
            })
            .expect("block traces did not contain trace for tx1");
        check_call_frame(
            call_frame1,
            alice,
            calculate_value1,
            expected_value1,
            *primary_contract.address(),
            *secondary_contract.address(),
        );
        return Ok(());
    }
}

#[test_multisetup([CURRENT_TO_L1])]
async fn debug_trace_call_js_tracer(tester: Tester) -> anyhow::Result<()> {
    let secondary_data = U256::from(7);
    let calculate_value = U256::from(3);
    let secondary_contract =
        TracingSecondary::deploy(tester.l2_provider.clone(), secondary_data).await?;
    let primary_contract =
        TracingPrimary::deploy(tester.l2_provider.clone(), *secondary_contract.address()).await?;

    let mut call_request = primary_contract
        .calculate(calculate_value)
        .into_transaction_request();

    let js_str = r#"
        {
           data: [],
           fault: function(log) {},
           step: function(log) {},
           enter: function (frame) {this.data.push(frame.getTo()); },
           result: function(ctx, db) { return this.data; }
        }"#;

    let mut opts = GethDebugTracingCallOptions::default();
    opts.tracing_options.tracer = Some(GethDebugTracerType::JsTracer(js_str.to_string()));
    call_request.max_priority_fee_per_gas = Some(1);
    call_request.max_fee_per_gas = Some(u128::MAX);
    call_request.set_from(tester.l2_wallet.default_signer().address());

    let trace = tester
        .l2_provider
        .debug_trace_call(call_request, BlockId::latest(), opts)
        .await?;

    let addresses = match trace {
        GethTrace::JS(value) => value
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_lowercase()))
                    .collect::<Vec<_>>()
            })
            .expect("tracer result missing addresses"),
        other => panic!("expected JS trace result, got {other:?}"),
    };

    let expected_primary = format!("{:#x}", primary_contract.address()).to_lowercase();
    let expected_secondary = format!("{:#x}", secondary_contract.address()).to_lowercase();

    assert!(
        addresses.iter().any(|addr| addr == &expected_primary),
        "primary contract address not found in tracer output"
    );
    assert!(
        addresses.iter().any(|addr| addr == &expected_secondary),
        "secondary contract address not found in tracer output"
    );

    Ok(())
}

#[test_multisetup([CURRENT_TO_L1])]
async fn debug_trace_call_js_tracer_with_db(tester: Tester) -> anyhow::Result<()> {
    let secondary_data = U256::from(7);
    let calculate_value = U256::from(3);
    let secondary_contract =
        TracingSecondary::deploy(tester.l2_provider.clone(), secondary_data).await?;
    let primary_contract =
        TracingPrimary::deploy(tester.l2_provider.clone(), *secondary_contract.address()).await?;

    let mut call_request = primary_contract
        .calculate(calculate_value)
        .into_transaction_request();

    let js_str = r#"
        {
            data: [],
            write: function (log) { this.data.push([log.address, log.key, log.value]); },
            result: function(ctx, db) { let [address, key, value] = this.data[this.data.length-1]; return [db.getState(address, key), value]; }
        }"#;

    let mut opts = GethDebugTracingCallOptions::default();
    opts.tracing_options.tracer = Some(GethDebugTracerType::JsTracer(js_str.to_string()));
    call_request.max_priority_fee_per_gas = Some(1);
    call_request.max_fee_per_gas = Some(u128::MAX);
    call_request.set_from(tester.l2_wallet.default_signer().address());

    let trace = tester
        .l2_provider
        .debug_trace_call(call_request, BlockId::latest(), opts)
        .await?;

    let values = match trace {
        GethTrace::JS(value) => value
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_lowercase()))
                    .collect::<Vec<_>>()
            })
            .expect("tracer result missing addresses"),
        other => panic!("expected JS trace result, got {other:?}"),
    };

    assert_eq!(values.len(), 2, "expected exactly two values from tracer");
    assert_eq!(
        values[0], values[1],
        "db.getState must return the same value as stored as a sanity check"
    );
    let res = secondary_data * calculate_value;
    assert_eq!(
        format!("{res:#x}").to_lowercase(),
        format!(
            "{:#x}",
            u128::from_str_radix(values[0].trim_start_matches("0x"), 16)?
        )
        .to_lowercase(),
        "stored value must match the expected one"
    );

    Ok(())
}

#[test_multisetup([CURRENT_TO_L1])]
async fn debug_trace_call_stack(tester: Tester) -> anyhow::Result<()> {
    let secondary_data = U256::from(7);
    let calculate_value = U256::from(3);
    let secondary_contract =
        TracingSecondary::deploy(tester.l2_provider.clone(), secondary_data).await?;
    let primary_contract =
        TracingPrimary::deploy(tester.l2_provider.clone(), *secondary_contract.address()).await?;

    let mut call_request = primary_contract
        .calculate(calculate_value)
        .into_transaction_request();

    let js_str = r#"
        {
          setup: function () {
            this.logs = [];
          },

          _bytesToHex: function (bytes) {
            var s = "0x";
            for (var i = 0; i < bytes.length; i++) {
              var h = bytes[i].toString(16);
              if (h.length === 1) h = "0" + h;
              s += h;
            }
            return s;
          },

          step: function (log, db) {
            var op = log.op.toString();
            var topicCount = { LOG0:0, LOG1:1, LOG2:2, LOG3:3, LOG4:4 }[op];
            if (topicCount === undefined) return;

            let stackTop = log.stack.peek(0);

            this.logs.push({
              depth: log.getDepth(),
              pc: log.getPC(),
              data: stackTop.toString(16),
            });
          },

          result: function () {
            return { type: "events", logs: this.logs };
          }
        }"#;

    let mut opts = GethDebugTracingCallOptions::default();
    opts.tracing_options.tracer = Some(GethDebugTracerType::JsTracer(js_str.to_string()));
    call_request.max_priority_fee_per_gas = Some(1);
    call_request.max_fee_per_gas = Some(u128::MAX);
    call_request.set_from(tester.l2_wallet.default_signer().address());

    let trace = tester
        .l2_provider
        .debug_trace_call(call_request, BlockId::latest(), opts)
        .await?;

    let val = match trace {
        GethTrace::JS(value) => value
            .as_object()
            .expect("tracer result missing addresses")
            .get("logs")
            .expect("geth tracer result missing data")
            .as_array()
            .expect("tracer logs is not an array")
            .first()
            .expect("tracer logs is empty")
            .as_object()
            .expect("tracer log entry is not an object")
            .get("data")
            .expect("tracer log entry missing data")
            .as_str()
            .expect("tracer log data is not a string")
            .to_string(),
        other => panic!("expected JS trace result, got {other:?}"),
    };

    let res = secondary_data * calculate_value;
    assert_eq!(
        format!("{res:#x}").to_lowercase(),
        format!(
            "{:#x}",
            u128::from_str_radix(val.trim_start_matches("0x"), 16)?
        )
        .to_lowercase(),
        "stored value must match the expected one"
    );

    Ok(())
}
