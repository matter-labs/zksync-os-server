use std::time::Duration;

use alloy::providers::Provider;
use alloy::{network::ReceiptResponse, primitives::Address};
use backon::{ConstantBuilder, Retryable};
use zksync_os_integration_tests::{Tester, assert_traits::ReceiptAssert, contracts::EventEmitter};

#[test_log::test(tokio::test)]
async fn transaction_replay() -> anyhow::Result<()> {
    let main_node = Tester::setup().await?;
    let en1 = main_node.launch_external_node().await?;

    let deploy_tx_receipt = EventEmitter::deploy_builder(main_node.l2_provider.clone())
        .send()
        .await?
        .expect_successful_receipt()
        .await?;
    let contract_address = deploy_tx_receipt
        .contract_address()
        .expect("no contract deployed");

    check_contract_present(&en1, contract_address).await?;

    let en2 = main_node.launch_external_node().await?;

    check_contract_present(&en2, contract_address).await?;

    let deploy_tx_receipt = EventEmitter::deploy_builder(main_node.l2_provider.clone())
        .send()
        .await?
        .expect_successful_receipt()
        .await?;
    let contract_address = deploy_tx_receipt
        .contract_address()
        .expect("no contract deployed");

    check_contract_present(&en1, contract_address).await?;
    check_contract_present(&en2, contract_address).await?;

    Ok(())
}

async fn check_contract_present(en: &Tester, contract_address: Address) -> anyhow::Result<()> {
    (|| async {
        let latest_code = en.l2_provider.get_code_at(contract_address).await?;
        if latest_code == EventEmitter::DEPLOYED_BYTECODE {
            Ok(())
        } else {
            Err(anyhow::anyhow!("deployed bytecode mismatch"))
        }
    })
    .retry(
        ConstantBuilder::default()
            .with_delay(Duration::from_secs(1))
            .with_max_times(10),
    )
    .await
}
