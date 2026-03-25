use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, U256, utils::parse_ether};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use anyhow::Result;
use tokio::time::{Duration, timeout};
use zksync_os_integration_tests::assert_traits::ReceiptAssert;
use zksync_os_integration_tests::contracts::EventEmitter;
use zksync_os_integration_tests::dyn_wallet_provider::EthWalletProvider;
use zksync_os_integration_tests::{DeploymentFilterConfig, GatewayTester};

/// The default rich wallet address (derived from the well-known test private key).
const AUTHORIZED_DEPLOYER: &str = "0x36615Cf349d7F6344891B1e7CA7C72883F5dc049";

/// Set up a multi-chain environment with deployment filter and fund an unauthorized account.
async fn setup() -> Result<(GatewayTester, Address)> {
    let authorized = AUTHORIZED_DEPLOYER.parse::<Address>().unwrap();
    let config = DeploymentFilterConfig {
        enabled: true,
        allowed_deployers: vec![authorized],
    };
    let mut mc = GatewayTester::builder()
        .deployment_filter(config)
        .num_chains(1)
        .build()
        .await?;

    let signer = PrivateKeySigner::random();
    let unauthorized = signer.address();
    mc.chains[0]
        .l2_provider
        .wallet_mut()
        .register_signer(signer);
    mc.chains[0]
        .l2_provider
        .send_transaction(
            TransactionRequest::default()
                .with_to(unauthorized)
                .with_value(parse_ether("1").unwrap()),
        )
        .await?
        .expect_successful_receipt()
        .await?;

    Ok((mc, unauthorized))
}

#[test_log::test(tokio::test)]
async fn unauthorized_address_deploy_is_rejected() -> Result<()> {
    let (mc, unauthorized) = setup().await?;

    // Rejected during block execution (FilteredByValidator -> Purge).
    let pending = EventEmitter::deploy_builder(mc.chain(0).l2_provider.clone())
        .from(unauthorized)
        .send()
        .await?;

    timeout(Duration::from_secs(1), pending.get_receipt())
        .await
        .expect_err("deploy from unauthorized address should not produce a receipt");

    Ok(())
}

#[test_log::test(tokio::test)]
async fn filter_only_blocks_deploys_not_transfers() -> Result<()> {
    let (mc, unauthorized) = setup().await?;

    mc.chain(0)
        .l2_provider
        .send_transaction(
            TransactionRequest::default()
                .with_from(unauthorized)
                .with_to(Address::random())
                .with_value(U256::from(1)),
        )
        .await?
        .expect_successful_receipt()
        .await?;

    Ok(())
}
