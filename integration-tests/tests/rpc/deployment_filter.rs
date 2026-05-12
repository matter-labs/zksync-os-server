use alloy::eips::Encodable2718;
use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, TxKind, U256, utils::parse_ether};
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

/// Multi-chain env with deployment filter, funds an unauthorized account, returns its signer.
async fn setup() -> Result<(GatewayTester, PrivateKeySigner)> {
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
    mc.chain_mut(0)
        .l2_provider
        .wallet_mut()
        .register_signer(signer.clone());
    mc.chain_mut(0)
        .l2_provider
        .send_transaction(
            TransactionRequest::default()
                .with_to(unauthorized)
                .with_value(parse_ether("1").unwrap()),
        )
        .await?
        .expect_successful_receipt()
        .await?;

    Ok((mc, signer))
}

#[test_log::test(tokio::test)]
async fn unauthorized_address_deploy_is_rejected() -> Result<()> {
    let (mc, signer) = setup().await?;
    let unauthorized = signer.address();

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
async fn send_raw_transaction_sync_surfaces_filter_rejection() -> Result<()> {
    // FilteredByValidator -> Purge -> failed_transactions on the main node.
    let (mc, signer) = setup().await?;
    let unauthorized = signer.address();
    let chain = mc.chain(0);
    let wallet = chain.l2_provider.wallet().clone();

    let fees = chain.l2_provider.estimate_eip1559_fees().await?;
    let tx = TransactionRequest::default()
        .from(unauthorized)
        .with_chain_id(chain.l2_provider.get_chain_id().await?)
        .with_kind(TxKind::Create)
        .with_input(EventEmitter::BYTECODE.clone())
        .with_nonce(
            chain
                .l2_provider
                .get_transaction_count(unauthorized)
                .await?,
        )
        .with_max_fee_per_gas(fees.max_fee_per_gas)
        .with_max_priority_fee_per_gas(fees.max_priority_fee_per_gas)
        .with_gas_limit(2_000_000);
    let tx_envelope = tx.build(&wallet).await?;
    let encoded = tx_envelope.encoded_2718();

    let error = chain
        .l2_provider
        .send_raw_transaction_sync(&encoded)
        .await
        .expect_err("deploy from unauthorized address should be rejected");
    let msg = error.to_string();
    assert!(
        msg.contains("rejected during execution") && msg.contains("FilteredByValidator"),
        "expected execution rejection error, got: {msg}"
    );

    Ok(())
}

#[test_log::test(tokio::test)]
async fn en_send_raw_transaction_sync_propagates_filter_rejection() -> Result<()> {
    // EN forwards eth_sendRawTransactionSync to main, so main's rejection reaches the caller.
    let (mc, signer) = setup().await?;
    let unauthorized = signer.address();
    let mut en = mc.chain(0).launch_external_node().await?;
    en.l2_provider.wallet_mut().register_signer(signer);
    let wallet = en.l2_provider.wallet().clone();

    let fees = en.l2_provider.estimate_eip1559_fees().await?;
    let tx = TransactionRequest::default()
        .from(unauthorized)
        .with_chain_id(en.l2_provider.get_chain_id().await?)
        .with_kind(TxKind::Create)
        .with_input(EventEmitter::BYTECODE.clone())
        .with_nonce(en.l2_provider.get_transaction_count(unauthorized).await?)
        .with_max_fee_per_gas(fees.max_fee_per_gas)
        .with_max_priority_fee_per_gas(fees.max_priority_fee_per_gas)
        .with_gas_limit(2_000_000);
    let tx_envelope = tx.build(&wallet).await?;
    let encoded = tx_envelope.encoded_2718();

    let error = en
        .l2_provider
        .send_raw_transaction_sync(&encoded)
        .await
        .expect_err("EN sync should propagate main node's rejection");
    let msg = error.to_string();
    assert!(
        msg.contains("rejected during execution") && msg.contains("FilteredByValidator"),
        "expected execution rejection error, got: {msg}"
    );

    Ok(())
}

#[test_log::test(tokio::test)]
async fn filter_only_blocks_deploys_not_transfers() -> Result<()> {
    let (mc, signer) = setup().await?;
    let unauthorized = signer.address();

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
