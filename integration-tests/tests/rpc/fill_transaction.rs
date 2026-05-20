use alloy::consensus::{BlobTransactionSidecar, BlobTransactionSidecarVariant, Transaction};
use alloy::eips::Encodable2718;
use alloy::network::TransactionBuilder;
use alloy::primitives::{Bytes, U128, U256, address};
use alloy::providers::Provider;
use alloy::rpc::types::{FillTransaction, TransactionRequest};
use alloy::transports::TransportResult;
use zksync_os_integration_tests::{CURRENT_TO_L1, TestEnvironment, Tester, test_multisetup};
use zksync_os_server::config::FeeConfig;
use zksync_os_types::ZkEnvelope;

const FIXED_BASE_FEE: u128 = 100_000_000;
const FILL_GAS_PRICE_SCALE_FACTOR: f64 = 2.0;
const EXPECTED_FILL_GAS_PRICE: u128 = 200_000_000;

async fn launch_with_fixed_gas_price(env: TestEnvironment) -> anyhow::Result<Tester> {
    let mut config = env.default_config().await?;
    config.fee_config = FeeConfig {
        native_price_usd: 3e-9,
        base_fee_override: Some(U128::from(FIXED_BASE_FEE)),
        native_per_gas: 100,
        pubdata_price_override: Some(U128::from(1_000_000u64)),
        native_price_override: Some(U128::from(1_000_000u64)),
        pubdata_price_cap: None,
    };
    config.rpc_config.gas_price_scale_factor = FILL_GAS_PRICE_SCALE_FACTOR;
    env.launch(config).await
}

async fn fill_transaction(
    tester: &Tester,
    request: TransactionRequest,
) -> TransportResult<FillTransaction<ZkEnvelope>> {
    tester
        .l2_provider
        .client()
        .request("eth_fillTransaction", (request,))
        .await
}

async fn expect_fill_transaction_to_fail(tester: &Tester, request: TransactionRequest, msg: &str) {
    let err = fill_transaction(tester, request)
        .await
        .expect_err("`eth_fillTransaction` should fail");
    assert!(
        err.to_string().contains(msg),
        "expected `eth_fillTransaction` to fail with '{msg}' but got: {err}"
    );
}

#[test_multisetup([CURRENT_TO_L1])]
async fn fill_transaction_fills_defaults(env: TestEnvironment) -> anyhow::Result<()> {
    let tester = launch_with_fixed_gas_price(env).await?;
    let from = tester.l2_wallet.default_signer().address();
    let to = address!("0xa5d85D1D865F89a23A95d4F5F74850f289Dbc5f9");
    let expected_nonce = tester
        .l2_provider
        .get_transaction_count(from)
        .pending()
        .await?;
    let expected_chain_id = tester.l2_provider.get_chain_id().await?;
    assert_eq!(
        tester.l2_provider.get_gas_price().await?,
        EXPECTED_FILL_GAS_PRICE
    );

    let filled = fill_transaction(
        &tester,
        TransactionRequest::default()
            .from(from)
            .to(to)
            .value(U256::ONE),
    )
    .await?;

    assert_eq!(filled.raw, Bytes::from(filled.tx.encoded_2718()));
    assert!(!filled.raw.is_empty());
    assert_eq!(filled.tx.chain_id(), Some(expected_chain_id));
    assert_eq!(filled.tx.nonce(), expected_nonce);
    assert_eq!(filled.tx.to(), Some(to));
    assert_eq!(filled.tx.value(), U256::ONE);
    assert!(filled.tx.gas_limit() > 0);
    assert_eq!(filled.tx.max_priority_fee_per_gas(), Some(0));
    assert_eq!(filled.tx.max_fee_per_gas(), EXPECTED_FILL_GAS_PRICE);

    Ok(())
}

#[test_multisetup([CURRENT_TO_L1])]
async fn fill_transaction_preserves_provided_fields(tester: Tester) -> anyhow::Result<()> {
    let from = tester.l2_wallet.default_signer().address();
    let to = address!("0xa5d85D1D865F89a23A95d4F5F74850f289Dbc5f9");
    let expected_chain_id = tester.l2_provider.get_chain_id().await?;

    let filled = fill_transaction(
        &tester,
        TransactionRequest::default()
            .from(from)
            .to(to)
            .nonce(7)
            .gas_limit(50_000)
            .gas_price(1_000_000_000)
            .with_chain_id(expected_chain_id + 1),
    )
    .await?;

    assert_eq!(filled.tx.chain_id(), Some(expected_chain_id));
    assert_eq!(filled.tx.nonce(), 7);
    assert_eq!(filled.tx.gas_limit(), 50_000);
    assert_eq!(filled.tx.gas_price(), Some(1_000_000_000));
    assert_eq!(filled.tx.max_priority_fee_per_gas(), None);

    Ok(())
}

#[test_multisetup([CURRENT_TO_L1])]
async fn fill_transaction_estimates_gas_without_balance(
    env: TestEnvironment,
) -> anyhow::Result<()> {
    let tester = launch_with_fixed_gas_price(env).await?;
    let unfunded = address!("0x38711eC715A5A32180427792Dc0e97f8E3303072");
    let to = address!("0xF8fF3e62E94807a5C687f418Fe36942dD3a24525");
    assert_eq!(tester.l2_provider.get_balance(unfunded).await?, U256::ZERO);

    let filled =
        fill_transaction(&tester, TransactionRequest::default().from(unfunded).to(to)).await?;

    assert_eq!(filled.tx.nonce(), 0);
    assert_eq!(filled.tx.value(), U256::ZERO);
    assert!(filled.tx.gas_limit() > 0);
    assert_eq!(filled.tx.max_fee_per_gas(), EXPECTED_FILL_GAS_PRICE);

    Ok(())
}

#[test_multisetup([CURRENT_TO_L1])]
async fn fill_transaction_fail(tester: Tester) -> anyhow::Result<()> {
    let from = tester.l2_wallet.default_signer().address();

    expect_fill_transaction_to_fail(
        &tester,
        TransactionRequest {
            from: Some(from),
            sidecar: Some(BlobTransactionSidecarVariant::Eip4844(
                BlobTransactionSidecar {
                    blobs: vec![],
                    commitments: vec![],
                    proofs: vec![],
                },
            )),
            ..Default::default()
        },
        "EIP-4844 transactions are not supported",
    )
    .await;

    expect_fill_transaction_to_fail(
        &tester,
        TransactionRequest {
            from: Some(from),
            authorization_list: Some(vec![]),
            ..Default::default()
        },
        "EIP-7702 transactions are not supported",
    )
    .await;

    expect_fill_transaction_to_fail(
        &tester,
        TransactionRequest {
            from: Some(from),
            gas_price: Some(100),
            max_fee_per_gas: Some(100),
            ..Default::default()
        },
        "both `gasPrice` and (`maxFeePerGas` or `maxPriorityFeePerGas`) specified",
    )
    .await;

    expect_fill_transaction_to_fail(
        &tester,
        TransactionRequest {
            from: Some(from),
            max_fee_per_gas: Some(1_000_000_001),
            max_priority_fee_per_gas: Some(1_000_000_002),
            ..Default::default()
        },
        "`maxPriorityFeePerGas` higher than `maxFeePerGas`",
    )
    .await;

    expect_fill_transaction_to_fail(
        &tester,
        TransactionRequest {
            from: Some(from),
            max_priority_fee_per_gas: Some(u128::MAX),
            ..Default::default()
        },
        "`maxPriorityFeePerGas` is too high",
    )
    .await;

    Ok(())
}
