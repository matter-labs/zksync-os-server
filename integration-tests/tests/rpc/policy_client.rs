use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use anyhow::Result;
use httpmock::{Method, MockServer};
use serde_json::json;
use tokio::time::Duration;
use zksync_os_integration_tests::assert_traits::ReceiptAssert;
use zksync_os_integration_tests::{GatewayTester, PolicyServiceConfig};
use zksync_os_tx_validators::deployment_filter::FORCE_DEPLOYER_ADDRESS;
use zksync_os_types::BOOTLOADER_FORMAL_ADDRESS;

fn policy_service(url: String) -> PolicyServiceConfig {
    PolicyServiceConfig {
        url: Some(url),
        request_timeout: Duration::from_secs(5),
        auth_token: None,
        protocol_version: "1".into(),
        min_protocol_version: None,
        bypass_from: vec![BOOTLOADER_FORMAL_ADDRESS, FORCE_DEPLOYER_ADDRESS],
    }
}

async fn setup(server_url: String) -> Result<GatewayTester> {
    GatewayTester::builder()
        .policy_service(policy_service(server_url))
        .num_chains(1)
        .build()
        .await
}

#[test_log::test(tokio::test)]
async fn allow_response_lets_tx_through() -> Result<()> {
    let server = MockServer::start_async().await;
    let allow_mock = server
        .mock_async(|when, then| {
            when.method(Method::POST).path("/admit");
            then.status(200).json_body(json!({ "allow": true }));
        })
        .await;

    let mc = setup(server.base_url()).await?;

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
        allow_mock.calls_async().await >= 1,
        "policy service should have been called at least once"
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
    let allow_mock = server
        .mock_async(|when, then| {
            when.method(Method::POST).path("/admit");
            then.status(200).json_body(json!({ "allow": true }));
        })
        .await;

    let mc = setup(server.base_url()).await?;
    allow_mock.delete_async().await;
    let deny_mock = deny_for_target(&server, TEST_DENY_TARGET).await;
    let allow_mock = server
        .mock_async(|when, then| {
            when.method(Method::POST).path("/admit");
            then.status(200).json_body(json!({ "allow": true }));
        })
        .await;

    // With M2, the deny lands synchronously at the RPC boundary — the
    // client never sees a tx hash.
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
    drop(allow_mock);

    Ok(())
}

#[test_log::test(tokio::test)]
async fn deny_response_rejects_eth_call() -> Result<()> {
    let server = MockServer::start_async().await;
    let allow_mock = server
        .mock_async(|when, then| {
            when.method(Method::POST).path("/admit");
            then.status(200).json_body(json!({ "allow": true }));
        })
        .await;

    let mc = setup(server.base_url()).await?;
    allow_mock.delete_async().await;
    let deny_mock = deny_for_target(&server, TEST_DENY_TARGET).await;
    let _fallback_allow = server
        .mock_async(|when, then| {
            when.method(Method::POST).path("/admit");
            then.status(200).json_body(json!({ "allow": true }));
        })
        .await;

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
    let allow_mock = server
        .mock_async(|when, then| {
            when.method(Method::POST).path("/admit");
            then.status(200).json_body(json!({ "allow": true }));
        })
        .await;

    let mc = setup(server.base_url()).await?;
    allow_mock.delete_async().await;
    let deny_mock = deny_for_target(&server, TEST_DENY_TARGET).await;
    let _fallback_allow = server
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
async fn deny_for_target(server: &MockServer, address: Address) -> httpmock::Mock<'_> {
    let target = format!("{address:#x}").to_ascii_lowercase();
    server
        .mock_async(move |when, then| {
            let target = target.clone();
            when.method(Method::POST)
                .path("/admit")
                .is_true(move |req| {
                    let body = req.body();
                    let parsed: serde_json::Value = match serde_json::from_slice(body.as_ref()) {
                        Ok(v) => v,
                        Err(_) => return false,
                    };
                    parsed
                        .get("to")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_ascii_lowercase() == target)
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

#[test_log::test(tokio::test)]
async fn allow_response_lets_eth_call_through() -> Result<()> {
    let server = MockServer::start_async().await;
    let allow_mock = server
        .mock_async(|when, then| {
            when.method(Method::POST).path("/admit");
            then.status(200).json_body(json!({ "allow": true }));
        })
        .await;

    let mc = setup(server.base_url()).await?;

    // Admit-allow should land the call on the VM; an empty-target call
    // executes cleanly against an EOA and returns empty bytes.
    let result = mc
        .chain(0)
        .l2_provider
        .call(TransactionRequest::default().with_to(Address::random()))
        .await?;
    assert!(result.is_empty(), "empty-target call returns empty bytes");

    assert!(
        allow_mock.calls_async().await >= 1,
        "eth_call must consult the policy service"
    );

    Ok(())
}

#[test_log::test(tokio::test)]
async fn allow_response_lets_eth_estimate_gas_through() -> Result<()> {
    let server = MockServer::start_async().await;
    let allow_mock = server
        .mock_async(|when, then| {
            when.method(Method::POST).path("/admit");
            then.status(200).json_body(json!({ "allow": true }));
        })
        .await;

    let mc = setup(server.base_url()).await?;

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
        allow_mock.calls_async().await >= 1,
        "eth_estimateGas must consult the policy service"
    );

    Ok(())
}
