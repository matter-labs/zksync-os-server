#![cfg(feature = "prover-tests")]

use alloy::eips::BlockId;
use alloy::network::TransactionBuilder;
use alloy::primitives::{Bytes, address, bytes};
use alloy::providers::Provider;
use alloy::providers::ext::{DebugApi, TraceApi};
use alloy::rpc::types::TransactionRequest;
use alloy::rpc::types::trace::geth::{CallConfig, GethDebugTracingOptions};
use zksync_os_integration_tests::Tester;
use zksync_os_integration_tests::assert_traits::ReceiptAssert;

// Taken from https://zksync-os-testnet-alpha.staging-scan-v2.zksync.dev/tx/0x06470d46ea9d6ff291c63fa4a4ded70c5fdd75a34221f4b2974dac1875a97335
const INITCODE: Bytes = bytes!(
    "0x61004a600081600b8239f336600060003760006000366000600060055af16000557f601038036010600039601038036000f3000000000000000000000000000000006000523d600060103e3d60100160006000f000"
);
// Taken from https://zksync-os-testnet-alpha.staging-scan-v2.zksync.dev/tx/0x9cd3c3ab73c71a7d51b4fca9cb649b52eb2741c713d9778ebea94223520dd800
const CALLDATA: Bytes = bytes!(
    "0x0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000010102"
);

#[test_log::test(tokio::test)]
async fn prover() -> anyhow::Result<()> {
    // Test that prover can successfully prove at least one batch
    let tester = Tester::builder().enable_prover().build().await?;

    // Test environment comes with some L1 transactions by default, so one batch should be provable
    // without any new transactions inside the test.
    tester.prover_api.wait_for_batch_proven(1).await?;

    let receipt = tester
        .l2_provider
        .send_transaction(TransactionRequest::default().with_deploy_code(INITCODE))
        .await?
        .expect_successful_receipt()
        .await?;
    println!("Deploy transaction receipt: {:?}", receipt);

    tester.prover_api.wait_for_batch_proven(2).await?;

    let contract = receipt.contract_address.unwrap();
    let receipt = tester
        .l2_provider
        .send_transaction(
            TransactionRequest::default()
                .to(contract)
                .with_input(CALLDATA),
        )
        .await?
        .expect_successful_receipt()
        .await?;
    println!("Call transaction receipt: {:?}", receipt);

    tester.prover_api.wait_for_batch_proven(3).await?;

    Ok(())
}
