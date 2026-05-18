use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use anyhow::Result;
use httpmock::{Method, MockServer};
use serde_json::json;
use std::sync::{Arc, Mutex};
use tokio::time::Duration;
use zksync_os_integration_tests::assert_traits::ReceiptAssert;
use zksync_os_integration_tests::contracts::{TracingPrimary, TracingSecondary};
use zksync_os_integration_tests::{GatewayTester, PolicyServiceConfig};
use zksync_os_tx_validators::deployment_filter::FORCE_DEPLOYER_ADDRESS;
use zksync_os_types::BOOTLOADER_FORMAL_ADDRESS;

fn policy_service(server: &MockServer) -> PolicyServiceConfig {
    PolicyServiceConfig {
        url: Some(
            format!("http://{}:{}", server.host(), server.port())
                .parse()
                .unwrap(),
        ),
        request_timeout: Duration::from_secs(5),
        protocol_version: "1".into(),
        expected_protocol_version: None,
        bypass_from: vec![BOOTLOADER_FORMAL_ADDRESS, FORCE_DEPLOYER_ADDRESS],
        auth_token: Some("test-token".into()),
    }
}

async fn setup(server: &MockServer) -> Result<GatewayTester> {
    GatewayTester::builder()
        .policy_service(policy_service(server))
        .num_chains(1)
        .build()
        .await
}

/// Install allow-everything mocks for both `/admit` and `/judge`. Used by
/// tests that don't care about the policy decisions and just need the chain
/// to come up cleanly.
async fn allow_admit_and_judge(server: &MockServer) -> [httpmock::Mock<'_>; 2] {
    let admit = server
        .mock_async(|when, then| {
            when.method(Method::POST).path("/admit");
            then.status(200).json_body(json!({ "allow": true }));
        })
        .await;
    let judge = server
        .mock_async(|when, then| {
            when.method(Method::POST).path("/judge");
            then.status(200).json_body(json!({ "allow": true }));
        })
        .await;
    [admit, judge]
}

#[test_log::test(tokio::test)]
async fn allow_response_lets_tx_through() -> Result<()> {
    let server = MockServer::start_async().await;
    let [admit_mock, judge_mock] = allow_admit_and_judge(&server).await;

    let mc = setup(&server).await?;

    mc.chain(0)
        .l2_provider
        .send_transaction(
            TransactionRequest::default()
                .with_to(Address::random())
                .with_value(U256::from(1)),
        )
        .await?
        .expect_successful_receipt()
        .await?;

    assert!(
        admit_mock.calls_async().await >= 1,
        "/admit should have been called at least once"
    );
    assert!(
        judge_mock.calls_async().await >= 1,
        "/judge should have been called at least once (RPC sim + block-build)"
    );

    Ok(())
}

/// The test wallet is pre-funded via an L1 priority tx and drives every
/// RPC-admit call the setup phase makes. We can't deny those calls without
/// breaking node bring-up, so the deny tests install an allow-mock first,
/// let setup finish, then swap the mock to deny for the test payload only.
///
/// The target address is the unambiguous signal: setup's `estimate_gas`
/// self-targets the wallet (beneficiary → beneficiary), while the test
/// payload targets this sentinel. That keeps any future setup-side admit
/// requests passing.
const TEST_DENY_TARGET: Address =
    alloy::primitives::address!("00000000000000000000000000000000deadbeef");

#[test_log::test(tokio::test)]
async fn deny_response_rejects_send_raw_transaction() -> Result<()> {
    let server = MockServer::start_async().await;
    let [setup_admit, setup_judge] = allow_admit_and_judge(&server).await;

    let mc = setup(&server).await?;
    setup_admit.delete_async().await;
    setup_judge.delete_async().await;

    let deny_mock = deny_for_target(&server, "/admit", TEST_DENY_TARGET).await;
    let _fallback = allow_admit_and_judge(&server).await;

    // The deny lands synchronously at the RPC boundary — the client never
    // sees a tx hash.
    let err = mc
        .chain(0)
        .l2_provider
        .send_transaction(
            TransactionRequest::default()
                .with_to(TEST_DENY_TARGET)
                .with_value(U256::from(1)),
        )
        .await
        .expect_err("denied sendRawTransaction should fail synchronously");

    let msg = err.to_string();
    assert!(
        msg.contains("policy service"),
        "expected policy deny message, got: {msg}"
    );

    assert!(
        deny_mock.calls_async().await >= 1,
        "the deny rule should have matched at least once"
    );

    Ok(())
}

#[test_log::test(tokio::test)]
async fn deny_response_rejects_eth_call() -> Result<()> {
    let server = MockServer::start_async().await;
    let [setup_admit, setup_judge] = allow_admit_and_judge(&server).await;

    let mc = setup(&server).await?;
    setup_admit.delete_async().await;
    setup_judge.delete_async().await;

    let deny_mock = deny_for_target(&server, "/admit", TEST_DENY_TARGET).await;
    let _fallback = allow_admit_and_judge(&server).await;

    let err = mc
        .chain(0)
        .l2_provider
        .call(
            TransactionRequest::default()
                .with_to(TEST_DENY_TARGET)
                .with_value(U256::from(1)),
        )
        .await
        .expect_err("denied eth_call should fail synchronously");
    let msg = err.to_string();
    assert!(
        msg.contains("policy service"),
        "expected policy deny message, got: {msg}"
    );

    assert!(
        deny_mock.calls_async().await >= 1,
        "eth_call must consult the policy service"
    );

    Ok(())
}

#[test_log::test(tokio::test)]
async fn deny_response_rejects_eth_estimate_gas() -> Result<()> {
    let server = MockServer::start_async().await;
    let [setup_admit, setup_judge] = allow_admit_and_judge(&server).await;

    let mc = setup(&server).await?;
    setup_admit.delete_async().await;
    setup_judge.delete_async().await;

    let deny_mock = deny_for_target(&server, "/admit", TEST_DENY_TARGET).await;
    let _fallback = allow_admit_and_judge(&server).await;

    let err = mc
        .chain(0)
        .l2_provider
        .estimate_gas(
            TransactionRequest::default()
                .with_to(TEST_DENY_TARGET)
                .with_value(U256::from(1)),
        )
        .await
        .expect_err("denied eth_estimateGas should fail synchronously");

    let msg = err.to_string();
    assert!(
        msg.contains("policy service"),
        "expected policy deny message, got: {msg}"
    );

    assert!(
        deny_mock.calls_async().await >= 1,
        "eth_estimateGas must consult the policy service"
    );

    Ok(())
}

/// Deny only admit requests whose payload targets `address`. Anything else
/// falls through to the allow-mock installed after it.
///
/// Body shape: `/admit` exposes a flat `to` field; `/judge` carries a
/// nested `trace.frame.callee` (root of the captured call tree). EOA-to-EOA
/// simulations produce no frames, so `/judge` denials in those scenarios
/// use [`deny_judge_for_signer`] instead.
async fn deny_for_target<'s>(
    server: &'s MockServer,
    path: &'static str,
    address: Address,
) -> httpmock::Mock<'s> {
    let target = format!("{address:#x}").to_ascii_lowercase();
    server
        .mock_async(move |when, then| {
            let target = target.clone();
            when.method(Method::POST).path(path).is_true(move |req| {
                let body = req.body();
                let parsed: serde_json::Value = match serde_json::from_slice(body.as_ref()) {
                    Ok(v) => v,
                    Err(_) => return false,
                };
                payload_targets(&parsed, &target)
            });
            then.status(200).json_body(json!({
                "allow": false,
                "ruleId": "integration_test",
                "reason": "denied by test mock"
            }));
        })
        .await
}

/// Deny only `/judge` requests whose `from` matches `signer`. Use this
/// when `/judge` is shared with concurrent block-build calls (e.g. setup
/// txs still in mempool) so the assertion doesn't false-pass on the
/// wrong tx.
async fn deny_judge_for_signer<'s>(server: &'s MockServer, signer: Address) -> httpmock::Mock<'s> {
    let signer_hex = format!("{signer:#x}").to_ascii_lowercase();
    server
        .mock_async(move |when, then| {
            let signer_hex = signer_hex.clone();
            when.method(Method::POST)
                .path("/judge")
                .is_true(move |req| {
                    let parsed: serde_json::Value =
                        match serde_json::from_slice(req.body().as_ref()) {
                            Ok(v) => v,
                            Err(_) => return false,
                        };
                    parsed
                        .get("from")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_ascii_lowercase() == signer_hex)
                        .unwrap_or(false)
                });
            then.status(200).json_body(json!({
                "allow": false,
                "ruleId": "integration_test",
                "reason": "denied by test mock"
            }));
        })
        .await
}

/// Total frames in the captured call tree rooted at `frame` (root + every
/// descendant). `/judge` ships a nested tree, not a flat list.
fn count_frames(frame: &serde_json::Value) -> usize {
    let children = frame
        .get("children")
        .and_then(|c| c.as_array())
        .map(|c| c.iter().map(count_frames).sum::<usize>())
        .unwrap_or(0);
    1 + children
}

/// Returns true if the request body addresses the denied target. Inspects
/// `to` for `/admit` (flat shape) and the trace's root frame `callee` for
/// `/judge` (nested tree shape).
fn payload_targets(parsed: &serde_json::Value, target: &str) -> bool {
    if let Some(to) = parsed.get("to").and_then(|v| v.as_str())
        && to.to_ascii_lowercase() == target
    {
        return true;
    }
    parsed
        .get("trace")
        .and_then(|t| t.get("frame"))
        .and_then(|frame| frame.get("callee"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_ascii_lowercase() == target)
        .unwrap_or(false)
}

#[test_log::test(tokio::test)]
async fn allow_response_lets_eth_call_through() -> Result<()> {
    let server = MockServer::start_async().await;
    let [admit_mock, judge_mock] = allow_admit_and_judge(&server).await;

    let mc = setup(&server).await?;

    // Admit + judge allow should land the call on the VM; an empty-target
    // call executes cleanly against an EOA and returns empty bytes.
    let result = mc
        .chain(0)
        .l2_provider
        .call(TransactionRequest::default().with_to(Address::random()))
        .await?;
    assert!(result.is_empty(), "empty-target call returns empty bytes");

    assert!(
        admit_mock.calls_async().await >= 1,
        "eth_call must consult /admit"
    );
    assert!(
        judge_mock.calls_async().await >= 1,
        "eth_call must consult /judge"
    );

    Ok(())
}

#[test_log::test(tokio::test)]
async fn allow_response_lets_eth_estimate_gas_through() -> Result<()> {
    let server = MockServer::start_async().await;
    let [admit_mock, judge_mock] = allow_admit_and_judge(&server).await;

    let mc = setup(&server).await?;

    let estimate = mc
        .chain(0)
        .l2_provider
        .estimate_gas(
            TransactionRequest::default()
                .with_to(Address::random())
                .with_value(U256::from(1)),
        )
        .await?;
    assert!(estimate > 0, "estimate_gas should return a positive value");

    assert!(
        admit_mock.calls_async().await >= 1,
        "eth_estimateGas must consult /admit"
    );
    assert!(
        judge_mock.calls_async().await >= 1,
        "eth_estimateGas must consult /judge"
    );

    Ok(())
}

// ---------- Regression: RPC-side /judge ships the captured trace ----------
//
// Routing the RPC simulation through `simulate_tx` (the call-simulation
// bootloader path) silently dropped the tracer's `on_new_execution_frame`
// callbacks for some real user-signed L2 txs, so `/judge` received an empty
// `frames` array. The Prividium policy service correctly takes its
// `judge.no_frames` allow path on empty traces — meaning denied txs were
// admitted at the RPC boundary anyway and only got rejected later, silently,
// at block-build. `simulate_and_judge` now routes through `run_block` to
// match the block-build trace; the test below pins it down by denying
// `/judge` for any request whose first frame's `callee` is the test
// contract. Without the fix the RPC body has zero frames → the deny mock
// can't match → tx is admitted → user gets a hash and silently waits.
// With the fix the deny matches at the RPC boundary and `send()` errors.

#[test_log::test(tokio::test)]
async fn rpc_judge_deny_matches_captured_trace_callee() -> Result<()> {
    let server = MockServer::start_async().await;
    let [setup_admit, setup_judge] = allow_admit_and_judge(&server).await;

    let mc = setup(&server).await?;

    // Deploy two contracts so the test call produces a 2-frame trace
    // (Primary.calculate -> Secondary.multiply). Allow everything during
    // deployment.
    let provider = mc.chain(0).l2_provider.clone();
    let secondary = TracingSecondary::deploy(provider.clone(), U256::from(0)).await?;
    let primary = TracingPrimary::deploy(provider.clone(), *secondary.address()).await?;
    let primary_address = *primary.address();

    // Swap setup mocks for a target-specific judge deny + catch-all allow.
    setup_admit.delete_async().await;
    setup_judge.delete_async().await;
    let deny_mock = deny_for_target(&server, "/judge", primary_address).await;
    let _fallback = allow_admit_and_judge(&server).await;

    // The body filter on `deny_mock` only matches when `frames[0].callee`
    // is the primary contract — the diagnostic that the RPC sim produced
    // the right frames at the RPC boundary, before the tx ever reaches
    // mempool / block-build.
    let res = primary.calculate(U256::from(7)).send().await;
    res.expect_err(
        "judge deny should fail send synchronously \
         (without the fix, RPC sim ships empty frames and the deny mock \
          doesn't match, so the tx is admitted to the mempool and only \
          gets purged silently at block-build)",
    );
    assert!(
        deny_mock.calls_async().await >= 1,
        "the targeted /judge deny rule should have matched at the RPC boundary"
    );

    Ok(())
}

/// Companion test: with `/judge` allowing everything but bodies captured,
/// the very first `/judge` body received during a contract-call window is
/// the RPC-side one (it fires before mempool insertion), and it must
/// contain the captured frames. Block-build's `/judge` body comes later
/// and is unaffected — this asserts the RPC-specific behaviour without
/// hand-rolling a body filter.
#[test_log::test(tokio::test)]
async fn rpc_judge_first_body_has_frames_for_contract_call() -> Result<()> {
    let server = MockServer::start_async().await;
    let _admit_allow = server
        .mock_async(|when, then| {
            when.method(Method::POST).path("/admit");
            then.status(200).json_body(json!({ "allow": true }));
        })
        .await;

    let captured: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
    let captured_for_mock = captured.clone();
    let _judge_allow = server
        .mock_async(move |when, then| {
            let captured = captured_for_mock.clone();
            when.method(Method::POST)
                .path("/judge")
                .is_true(move |req| {
                    captured.lock().unwrap().push(req.body().as_ref().to_vec());
                    true
                });
            then.status(200).json_body(json!({ "allow": true }));
        })
        .await;

    let mc = setup(&server).await?;
    let provider = mc.chain(0).l2_provider.clone();
    let secondary = TracingSecondary::deploy(provider.clone(), U256::from(0)).await?;
    let primary = TracingPrimary::deploy(provider.clone(), *secondary.address()).await?;
    let bodies_before = captured.lock().unwrap().len();

    primary
        .calculate(U256::from(7))
        .send()
        .await?
        .expect_successful_receipt()
        .await?;

    let first_body_during_test_call = {
        let all = captured.lock().unwrap();
        all.get(bodies_before)
            .cloned()
            .expect("expected at least one /judge body during the test call window")
    };
    let parsed: serde_json::Value = serde_json::from_slice(&first_body_during_test_call)?;
    let frames = parsed
        .get("trace")
        .and_then(|t| t.get("frame"))
        .map(count_frames)
        .unwrap_or(0);
    assert!(
        frames >= 2,
        "first /judge body during the test call window (the RPC-side call) \
         must carry >= 2 frames (Primary.calculate + Secondary.multiply); \
         got {frames}"
    );

    Ok(())
}

/// Same defect, eth_call surface: with the broken `PolicyTracer::finish_tx`
/// the captured frames were wiped by the bootloader-fired tracer.finish_tx
/// before `tracer.drain_frames()` could read them, so `eth_call` shipped
/// `frames: []` to `/judge`. After the fix the eth_call path also carries
/// the captured frames; a `/judge` deny scoped by `frames[0].callee` lands
/// the deny synchronously instead of letting the call return cleanly.
#[test_log::test(tokio::test)]
async fn rpc_judge_deny_blocks_eth_call_at_callee() -> Result<()> {
    let server = MockServer::start_async().await;
    let [setup_admit, setup_judge] = allow_admit_and_judge(&server).await;

    let mc = setup(&server).await?;

    let provider = mc.chain(0).l2_provider.clone();
    let secondary = TracingSecondary::deploy(provider.clone(), U256::from(0)).await?;
    let primary = TracingPrimary::deploy(provider.clone(), *secondary.address()).await?;
    let primary_address = *primary.address();

    setup_admit.delete_async().await;
    setup_judge.delete_async().await;
    let deny_mock = deny_for_target(&server, "/judge", primary_address).await;
    let _fallback = allow_admit_and_judge(&server).await;

    // `eth_call` of `Primary.calculate(7)` produces a 2-frame trace; the
    // deny mock matches `frames[0].callee == primary`, so a working judge
    // path lands a synchronous JSON-RPC error.
    let res = primary.calculate(U256::from(7)).call().await;
    res.expect_err(
        "judge deny should fail eth_call synchronously \
         (regression: previously the RPC sim shipped empty frames so the \
          deny mock couldn't match and the call returned cleanly)",
    );
    assert!(
        deny_mock.calls_async().await >= 1,
        "the targeted /judge deny rule should have matched at the eth_call boundary"
    );

    Ok(())
}

// ---------- L1 priority + upgrade requests bypass policy entirely ----------
//
// Block-build never fires the validator for L1 priority or upgrade txs
// (the bootloader's `process_l1_transaction` doesn't call begin/finish
// hooks); the RPC layer must skip too for consistency. `eth_call` with
// `transaction_type=0x7f` (L1 priority) is the canonical way to hit the
// non-L2 simulation path.

#[test_log::test(tokio::test)]
async fn l1_priority_eth_call_skips_admit_and_judge() -> Result<()> {
    let server = MockServer::start_async().await;
    let [admit_mock, judge_mock] = allow_admit_and_judge(&server).await;

    let mc = setup(&server).await?;

    // Snapshot post-setup call counts; the test call must not increment.
    let admit_before = admit_mock.calls_async().await;
    let judge_before = judge_mock.calls_async().await;

    let req = TransactionRequest::default()
        .with_to(Address::random())
        .transaction_type(0x7f); // L1PriorityTxType::TX_TYPE
    // We don't care whether simulation succeeds — only that admit/judge
    // weren't consulted.
    let _ = mc.chain(0).l2_provider.call(req).await;

    assert_eq!(
        admit_mock.calls_async().await,
        admit_before,
        "/admit must not fire for L1 priority requests"
    );
    assert_eq!(
        judge_mock.calls_async().await,
        judge_before,
        "/judge must not fire for L1 priority requests"
    );

    Ok(())
}

// ---------- /judge denial at the RPC boundary ----------
//
// `/admit` allows but `/judge` denies. Contract: the deny lands as a
// synchronous JSON-RPC error from the RPC method, not a silent drop the
// client has to discover by polling for a receipt.

#[test_log::test(tokio::test)]
async fn judge_deny_rejects_send_raw_transaction() -> Result<()> {
    let server = MockServer::start_async().await;
    let [setup_admit, setup_judge] = allow_admit_and_judge(&server).await;

    let mc = setup(&server).await?;
    let signer = mc.chain(0).l2_wallet.default_signer().address();
    setup_admit.delete_async().await;
    setup_judge.delete_async().await;
    // Deny only `/judge` calls whose `from` is the test signer. Avoids
    // false-passing on a setup tx still draining through block-build.
    let deny_mock = deny_judge_for_signer(&server, signer).await;
    let _judge_allow = server
        .mock_async(|when, then| {
            when.method(Method::POST).path("/judge");
            then.status(200).json_body(json!({ "allow": true }));
        })
        .await;
    let _admit_allow = server
        .mock_async(|when, then| {
            when.method(Method::POST).path("/admit");
            then.status(200).json_body(json!({ "allow": true }));
        })
        .await;

    let err = mc
        .chain(0)
        .l2_provider
        .send_transaction(
            TransactionRequest::default()
                .with_to(TEST_DENY_TARGET)
                .with_value(U256::from(1)),
        )
        .await
        .expect_err("judge-denied sendRawTransaction should fail synchronously");

    let msg = err.to_string();
    assert!(
        msg.contains("policy service"),
        "expected policy deny message, got: {msg}"
    );

    assert!(
        deny_mock.calls_async().await >= 1,
        "the judge deny rule should have matched"
    );

    Ok(())
}

#[test_log::test(tokio::test)]
async fn judge_deny_rejects_eth_call() -> Result<()> {
    let server = MockServer::start_async().await;
    let [setup_admit, setup_judge] = allow_admit_and_judge(&server).await;

    let mc = setup(&server).await?;
    let signer = mc.chain(0).l2_wallet.default_signer().address();
    setup_admit.delete_async().await;
    setup_judge.delete_async().await;

    // EOA-to-EOA eth_call simulations produce an empty trace, so the
    // body filter scopes on `from` (the request signer) instead.
    let deny_mock = deny_judge_for_signer(&server, signer).await;
    let _judge_allow = server
        .mock_async(|when, then| {
            when.method(Method::POST).path("/judge");
            then.status(200).json_body(json!({ "allow": true }));
        })
        .await;
    let _admit_allow = server
        .mock_async(|when, then| {
            when.method(Method::POST).path("/admit");
            then.status(200).json_body(json!({ "allow": true }));
        })
        .await;

    let err = mc
        .chain(0)
        .l2_provider
        .call(TransactionRequest::default().with_to(TEST_DENY_TARGET))
        .await
        .expect_err("judge-denied eth_call should fail synchronously");
    let msg = err.to_string();
    assert!(
        msg.contains("policy service"),
        "expected policy deny message, got: {msg}"
    );

    assert!(
        deny_mock.calls_async().await >= 1,
        "eth_call must consult /judge"
    );

    Ok(())
}

#[test_log::test(tokio::test)]
async fn judge_deny_rejects_eth_estimate_gas() -> Result<()> {
    let server = MockServer::start_async().await;
    let [setup_admit, setup_judge] = allow_admit_and_judge(&server).await;

    let mc = setup(&server).await?;
    let signer = mc.chain(0).l2_wallet.default_signer().address();
    setup_admit.delete_async().await;
    setup_judge.delete_async().await;
    let deny_mock = deny_judge_for_signer(&server, signer).await;
    let _judge_allow = server
        .mock_async(|when, then| {
            when.method(Method::POST).path("/judge");
            then.status(200).json_body(json!({ "allow": true }));
        })
        .await;
    let _admit_allow = server
        .mock_async(|when, then| {
            when.method(Method::POST).path("/admit");
            then.status(200).json_body(json!({ "allow": true }));
        })
        .await;

    let err = mc
        .chain(0)
        .l2_provider
        .estimate_gas(
            TransactionRequest::default()
                .with_to(TEST_DENY_TARGET)
                .with_value(U256::from(1)),
        )
        .await
        .expect_err("judge-denied eth_estimateGas should fail synchronously");
    let msg = err.to_string();
    assert!(
        msg.contains("policy service"),
        "expected policy deny message, got: {msg}"
    );

    assert!(
        deny_mock.calls_async().await >= 1,
        "eth_estimateGas must consult /judge"
    );

    Ok(())
}
