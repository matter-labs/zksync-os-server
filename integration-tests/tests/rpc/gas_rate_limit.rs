use alloy::eips::Encodable2718;
use alloy::network::{Ethereum, NetworkTransactionBuilder, TransactionBuilder};
use alloy::primitives::{Address, U256, utils::parse_ether};
use alloy::providers::{PendingTransactionBuilder, Provider};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use alloy::transports::TransportResult;
use anyhow::Result;
use std::num::NonZeroU64;
use std::time::Duration;
use tokio::time::{Instant, sleep, timeout};
use zksync_os_integration_tests::CURRENT_TO_L1;
use zksync_os_integration_tests::assert_traits::ReceiptAssert;
use zksync_os_provider::{EthWalletProvider, NodeProvider};

/// EIP-1474 "Limit exceeded", returned when the executed-gas rate limiter's bank is exhausted.
const RATE_LIMIT_ERROR_CODE: i64 = -32005;

/// The default rich wallet address (derived from the well-known test private key).
/// Configured as the exempt sender, so funding txs can never be rate-limited.
const RICH_WALLET: &str = "0x36615Cf349d7F6344891B1e7CA7C72883F5dc049";

/// 100k gas/s refill with max credit capped at 1s (100k gas); defaults for the rest.
/// A 10-transfer burst (210k executed gas) reliably exhausts the bank while staying
/// under the mempool's 16-per-sender slot limit even with a few probe txs in flight.
const GAS_PER_SECOND: u64 = 100_000;
const MAX_CREDIT_SECONDS: f64 = 1.0;
const BURST_TRANSFERS: u64 = 10;

/// Fee/chain parameters fetched once: they are stable for the test's lifetime, and
/// re-fetching them inside the polling loops would widen the timing windows.
struct TxParams {
    chain_id: u64,
    max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
}

impl TxParams {
    async fn fetch(provider: &NodeProvider) -> TransportResult<Self> {
        let chain_id = provider.get_chain_id().await?;
        let fees = provider.estimate_eip1559_fees().await?;
        Ok(Self {
            chain_id,
            max_fee_per_gas: fees.max_fee_per_gas,
            max_priority_fee_per_gas: fees.max_priority_fee_per_gas,
        })
    }
}

/// Explicit nonces keep rate-limited attempts from desyncing alloy's cached nonce manager.
async fn send_transfer_with_nonce(
    provider: &NodeProvider,
    params: &TxParams,
    from: Address,
    to: Address,
    value: U256,
    nonce: u64,
) -> TransportResult<PendingTransactionBuilder<Ethereum>> {
    let tx = TransactionRequest::default()
        .from(from)
        .with_to(to)
        .with_value(value)
        .with_nonce(nonce)
        .with_chain_id(params.chain_id)
        .with_gas_limit(21_000)
        .with_max_fee_per_gas(params.max_fee_per_gas)
        .with_max_priority_fee_per_gas(params.max_priority_fee_per_gas);
    let envelope = tx
        .build(provider.wallet())
        .await
        .expect("signing with a registered signer cannot fail");
    provider
        .send_raw_transaction(&envelope.encoded_2718())
        .await
}

/// Like [`send_transfer_with_nonce`] but with the sender's current pending nonce.
async fn send_transfer(
    provider: &NodeProvider,
    params: &TxParams,
    from: Address,
    to: Address,
    value: U256,
) -> TransportResult<PendingTransactionBuilder<Ethereum>> {
    let nonce = provider.get_transaction_count(from).pending().await?;
    send_transfer_with_nonce(provider, params, from, to, value, nonce).await
}

#[test_log::test(tokio::test)]
async fn gas_rate_limiter_closes_gate_and_recovers() -> Result<()> {
    let limited_signer = PrivateKeySigner::random();
    let limited = limited_signer.address();
    let rich: Address = RICH_WALLET.parse().unwrap();

    let env = CURRENT_TO_L1.environment().await?;
    let mut config = env.default_config().await?;
    config.rpc_config.tx_gas_rate_limit.gas_per_second = NonZeroU64::new(GAS_PER_SECOND);
    config.rpc_config.tx_gas_rate_limit.max_credit_seconds = MAX_CREDIT_SECONDS;
    config.rpc_config.tx_gas_rate_limit.exempt_senders = [rich].into_iter().collect();
    let mut tester = env.launch(config).await?;
    tester
        .l2_provider
        .wallet_mut()
        .register_signer(limited_signer);

    let provider = tester.l2_provider.clone();
    let sink = Address::repeat_byte(0x42);
    let params = TxParams::fetch(&provider).await?;

    // Fund the non-exempt account; the rich sender is exempt so this cannot be rate-limited.
    send_transfer(&provider, &params, rich, limited, parse_ether("1")?)
        .await?
        .expect_successful_receipt()
        .await?;

    // Burst: executed gas (10 × 21k = 210k) far exceeds the bank's max credit (100k),
    // so the gate must close once these blocks seal.
    let first_nonce = provider.get_transaction_count(limited).pending().await?;
    for i in 0..BURST_TRANSFERS {
        let _pending = send_transfer_with_nonce(
            &provider,
            &params,
            limited,
            sink,
            U256::from(1),
            first_nonce + i,
        )
        .await
        .expect("burst txs are admitted while the gate is open");
    }

    // The drain is block-granular: poll until a submission is rejected with -32005.
    let deadline = Instant::now() + Duration::from_secs(15);
    let rate_limit_error = loop {
        match send_transfer(&provider, &params, limited, sink, U256::from(1)).await {
            Err(err) => {
                let resp = err
                    .as_error_resp()
                    .unwrap_or_else(|| panic!("expected JSON-RPC error response, got: {err:?}"))
                    .clone();
                assert_eq!(
                    resp.code, RATE_LIMIT_ERROR_CODE,
                    "unexpected error while waiting for the gate to close: {resp:?}"
                );
                break resp;
            }
            Ok(_) => {
                assert!(
                    Instant::now() < deadline,
                    "gate did not close within 15s of exhausting the bank"
                );
                sleep(Duration::from_millis(250)).await;
            }
        }
    };
    assert!(
        rate_limit_error.message.contains("gas rate limit"),
        "unexpected message: {}",
        rate_limit_error.message
    );
    let retry_data = rate_limit_error
        .data
        .as_ref()
        .expect("rate limit error carries retry data")
        .to_string();
    assert!(
        retry_data.contains("retryAfterMs"),
        "unexpected data: {retry_data}"
    );

    // While the gate is closed, exempt senders are unaffected end-to-end.
    let exempt_pending = send_transfer(&provider, &params, rich, sink, U256::from(1))
        .await
        .expect("exempt sender must bypass the rate limiter");
    timeout(
        Duration::from_secs(10),
        exempt_pending.expect_successful_receipt(),
    )
    .await
    .expect("exempt tx should be mined while the gate is closed")?;

    // Non-exempt traffic stays rejected until the deficit is repaid, then is accepted again.
    let deadline = Instant::now() + Duration::from_secs(30);
    let reopened = loop {
        match send_transfer(&provider, &params, limited, sink, U256::from(1)).await {
            Ok(pending) => break pending,
            Err(err) => {
                let resp = err
                    .as_error_resp()
                    .unwrap_or_else(|| panic!("expected JSON-RPC error response, got: {err:?}"));
                assert_eq!(resp.code, RATE_LIMIT_ERROR_CODE);
                assert!(Instant::now() < deadline, "gate did not reopen within 30s");
                sleep(Duration::from_millis(500)).await;
            }
        }
    };
    timeout(
        Duration::from_secs(10),
        reopened.expect_successful_receipt(),
    )
    .await
    .expect("tx accepted after reopening should be mined")?;

    Ok(())
}
