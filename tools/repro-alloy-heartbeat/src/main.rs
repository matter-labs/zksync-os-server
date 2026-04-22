use alloy::node_bindings::Anvil;
use alloy::primitives::U256;
use alloy::network::{ReceiptResponse, TransactionBuilder};
use alloy::providers::ext::AnvilApi;
use alloy::providers::{PendingTransactionError, Provider, ProviderBuilder, WalletProvider};
use alloy::rpc::client::RpcClient;
use alloy::rpc::types::TransactionRequest;
use clap::Parser;
use std::time::Duration;
use tracing::{info, warn};

#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// Number of attempts before giving up.
    #[arg(long, default_value_t = 50)]
    attempts: usize,
    /// Confirmations requested from Alloy.
    #[arg(long, default_value_t = 3)]
    confirmations: u64,
    /// Timeout passed to PendingTransactionBuilder.
    #[arg(long, default_value_t = 10_000)]
    timeout_ms: u64,
    /// Poll interval used by the Alloy client.
    #[arg(long, default_value_t = 100)]
    poll_interval_ms: u64,
    /// How many blocks to mine immediately after registering the watcher.
    #[arg(long, default_value_t = 3)]
    burst_blocks: u64,
    /// Small delay to give the watcher time to register before mining.
    #[arg(long, default_value_t = 10)]
    register_wait_ms: u64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG")
                .unwrap_or_else(|_| "info,alloy_provider::heart=warn".to_owned()),
        )
        .init();

    let args = Args::parse();
    let anvil = Anvil::new().block_time(1).spawn();
    let wallet = anvil
        .wallet()
        .expect("anvil should expose a dev wallet");

    let client = RpcClient::builder()
        .connect(&anvil.endpoint())
        .await?
        .with_poll_interval(Duration::from_millis(args.poll_interval_ms));
    let provider = ProviderBuilder::new()
        .wallet(wallet)
        .connect_client(client);

    provider.anvil_set_auto_mine(false).await?;

    info!(
        endpoint = %anvil.endpoint(),
        attempts = args.attempts,
        confirmations = args.confirmations,
        timeout_ms = args.timeout_ms,
        poll_interval_ms = args.poll_interval_ms,
        burst_blocks = args.burst_blocks,
        "starting alloy heartbeat repro",
    );

    for attempt in 1..=args.attempts {
        let confirmations = args.confirmations;
        let timeout_ms = args.timeout_ms;
        let tx = TransactionRequest::default()
            .with_to(provider.default_signer_address())
            .with_value(U256::from(1_u64));
        let pending = provider.send_transaction(tx).await?;
        let tx_hash = *pending.tx_hash();

        let watcher = tokio::spawn(async move {
            pending
                .with_required_confirmations(confirmations)
                .with_timeout(Some(Duration::from_millis(timeout_ms)))
                .get_receipt()
                .await
        });

        tokio::time::sleep(Duration::from_millis(args.register_wait_ms)).await;
        provider.anvil_mine(Some(args.burst_blocks), None).await?;

        let result = watcher.await.expect("watch task panicked");
        let receipt = provider.get_transaction_receipt(tx_hash).await?;
        let latest_block = provider.get_block_number().await?;

        match (&result, &receipt) {
            (Err(PendingTransactionError::TxWatcher(_)), Some(receipt)) => {
                warn!(
                    attempt,
                    %tx_hash,
                    latest_block,
                    receipt_block = ?receipt.block_number(),
                    receipt_status = receipt.status(),
                    error = %result.as_ref().unwrap_err(),
                    "reproduced: Alloy watcher timed out even though receipt exists",
                );
                return Ok(());
            }
            _ => {
                info!(
                    attempt,
                    %tx_hash,
                    latest_block,
                    watcher_ok = result.is_ok(),
                    receipt_present = receipt.is_some(),
                    receipt_block = ?receipt.as_ref().and_then(|r| r.block_number()),
                    "attempt completed",
                );
            }
        }
    }

    anyhow::bail!(
        "did not reproduce in {} attempts; try increasing --attempts or --burst-blocks",
        args.attempts
    )
}
