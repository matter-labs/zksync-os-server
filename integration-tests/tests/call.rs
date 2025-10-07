use alloy::consensus::BlobTransactionSidecar;
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use alloy::rpc::types::state::StateOverride;
use zksync_os_integration_tests::Tester;
use zksync_os_integration_tests::assert_traits::EthCallAssert;
use zksync_os_integration_tests::contracts::{EventEmitter, SimpleRevert};

#[test_log::test(tokio::test)]
async fn call_genesis() -> anyhow::Result<()> {
    // Test that the node can run `eth_call` on genesis
    let tester = Tester::setup().await?;
    tester
        .l2_provider
        .call(TransactionRequest::default())
        .block(0.into())
        .await?;
    Ok(())
}

#[test_log::test(tokio::test)]
async fn call_fail() -> anyhow::Result<()> {
    // Test that the node responds with proper errors when `eth_call` fails
    let tester = Tester::setup().await?;

    // Override errors
    tester
        .l2_provider
        .call(TransactionRequest::default())
        .overrides(StateOverride::from_iter([]))
        .expect_to_fail("state overrides are not supported in `eth_call`")
        .await;

    // Tx type errors
    tester
        .l2_provider
        .call(TransactionRequest {
            sidecar: Some(BlobTransactionSidecar {
                blobs: vec![],
                commitments: vec![],
                proofs: vec![],
            }),
            ..Default::default()
        })
        .expect_to_fail("EIP-4844 transactions are not supported")
        .await;
    tester
        .l2_provider
        .call(TransactionRequest {
            authorization_list: Some(vec![]),
            ..Default::default()
        })
        .expect_to_fail("EIP-7702 transactions are not supported")
        .await;

    // Block not found errors
    tester
        .l2_provider
        .call(TransactionRequest::default())
        // Very far ahead block
        .block((u32::MAX as u64).into())
        .expect_to_fail("block not found")
        .await;

    // Fee errors
    tester
        .l2_provider
        .call(TransactionRequest {
            gas_price: Some(100),
            max_fee_per_gas: Some(100),
            ..Default::default()
        })
        .expect_to_fail("both `gasPrice` and (`maxFeePerGas` or `maxPriorityFeePerGas`) specified")
        .await;
    tester
        .l2_provider
        .call(TransactionRequest {
            max_fee_per_gas: Some(1),
            max_priority_fee_per_gas: Some(1),
            ..Default::default()
        })
        .expect_to_fail("`maxFeePerGas` less than `block.baseFee`")
        .await;
    tester
        .l2_provider
        .call(TransactionRequest {
            max_fee_per_gas: Some(1001),
            max_priority_fee_per_gas: Some(1002),
            ..Default::default()
        })
        .expect_to_fail("`maxPriorityFeePerGas` higher than `maxFeePerGas`")
        .await;
    tester
        .l2_provider
        .call(TransactionRequest {
            max_priority_fee_per_gas: Some(u128::MAX),
            ..Default::default()
        })
        .expect_to_fail("`maxPriorityFeePerGas` is too high")
        .await;

    // Missing field errors
    tester
        .l2_provider
        .call(TransactionRequest {
            max_fee_per_gas: Some(1001),
            ..Default::default()
        })
        .expect_to_fail("missing `maxPriorityFeePerGas` field for EIP-1559 transaction")
        .await;

    Ok(())
}

#[test_log::test(tokio::test)]
async fn call_deploy() -> anyhow::Result<()> {
    // Test that the node can run `eth_call` with contract deployment
    let tester = Tester::setup().await?;
    let result = EventEmitter::deploy_builder(tester.l2_provider.clone())
        .call()
        .await?;
    assert_eq!(result, EventEmitter::DEPLOYED_BYTECODE);
    Ok(())
}

#[test_log::test(tokio::test)]
async fn call_revert() -> anyhow::Result<()> {
    // Test that the node returns error on reverting `eth_call`
    let tester = Tester::setup().await?;

    let simple_revert = SimpleRevert::deploy(tester.l2_provider.clone()).await?;
    // Custom error is returned as accompanying data
    let error = simple_revert
        .simpleRevert()
        .call_raw()
        .await
        .expect_err("call did not result in revert error")
        .to_string();
    assert_eq!(
        error,
        "server returned an error response: error code 3: execution reverted, data: \"0xc2bb947c\""
    );
    // String reverts are parsed out as a revert reason
    let error = simple_revert
        .stringRevert()
        .call_raw()
        .await
        .expect_err("call did not result in revert error")
        .to_string();
    assert_eq!(
        error,
        "server returned an error response: error code 3: execution reverted: my message, data: \"0x08c379a00000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000000000000000000a6d79206d65737361676500000000000000000000000000000000000000000000\""
    );

    Ok(())
}
