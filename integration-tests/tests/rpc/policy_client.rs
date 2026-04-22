use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use anyhow::Result;
use httpmock::{Method, MockServer};
use serde_json::json;
use tokio::time::Duration;
use zksync_os_integration_tests::assert_traits::ReceiptAssert;
use zksync_os_integration_tests::provider::ZksyncTestingProvider;
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

#[test_log::test(tokio::test)]
async fn deny_response_filters_tx() -> Result<()> {
    let server = MockServer::start_async().await;

    // Deny every admit call. Protocol-internal txs never reach the service
    // because their `from` is on the bypass allowlist, so denying blanket
    // here is safe for startup.
    let deny_mock = server
        .mock_async(|when, then| {
            when.method(Method::POST).path("/admit");
            then.status(200).json_body(json!({
                "allow": false,
                "ruleId": "integration_test",
                "reason": "denied by test mock"
            }));
        })
        .await;

    let mc = setup(server.base_url()).await?;
    let block_at_submission = mc.chain(0).l2_zk_provider.get_block_number().await?;

    let pending = mc
        .chain(0)
        .l2_provider
        .send_transaction(
            TransactionRequest::default()
                .with_to(Address::random())
                .with_value(U256::from(1)),
        )
        .await?;
    let tx_hash = *pending.tx_hash();

    // A tx rejected by the validator is purged — never retried. Wait for
    // the chain to produce enough blocks past submission that the
    // sequencer has had multiple opportunities to include the tx; if it's
    // missing after that, it's gone for good. Block-based synchronisation
    // is immune to CI-speed flakiness that a fixed sleep or receipt
    // timeout would inherit.
    mc.chain(0)
        .l2_zk_provider
        .wait_for_block(block_at_submission + 3)
        .await?;

    let receipt = mc
        .chain(0)
        .l2_zk_provider
        .get_transaction_receipt(tx_hash)
        .await?;
    assert!(receipt.is_none(), "denied tx should not produce a receipt");

    assert!(
        deny_mock.calls_async().await >= 1,
        "the deny rule should have matched at least once"
    );

    Ok(())
}
