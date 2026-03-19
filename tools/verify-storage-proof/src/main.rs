use std::time::Duration;

use alloy::primitives::{Address, B256};
use alloy::providers::ProviderBuilder;
use clap::Parser;
use zksync_os_verify_storage_proof::{VerifyParams, verify_storage_proof};

#[derive(Parser)]
#[command(
    name = "verify-storage-proof",
    about = "Verify ZKsync storage slot values against L1 batch commitments"
)]
struct Args {
    /// L2 JSON-RPC endpoint
    #[arg(long)]
    l2_rpc: String,

    /// L1 JSON-RPC endpoint
    #[arg(long)]
    l1_rpc: String,

    /// L1 batch number
    #[arg(long)]
    batch_number: u64,

    /// Diamond proxy address on L1 (skips auto-discovery)
    #[arg(long)]
    l1_contract: Option<Address>,

    /// Bridgehub address on L1 (for auto-discovery of diamond proxy)
    #[arg(long)]
    bridgehub: Option<Address>,

    /// Seconds to wait for batch commitment on L1 (0 = fail immediately)
    #[arg(long, default_value = "60", value_name = "SECS")]
    commit_timeout: u64,

    /// Account address to prove storage for
    address: Address,

    /// Storage keys to verify (comma-separated)
    #[arg(value_delimiter = ',')]
    keys: Vec<B256>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();

    let commit_timeout = if args.commit_timeout > 0 {
        Some(Duration::from_secs(args.commit_timeout))
    } else {
        None
    };

    let l1_provider = ProviderBuilder::new().connect(&args.l1_rpc).await?;
    let l2_provider = ProviderBuilder::new().connect(&args.l2_rpc).await?;

    let result = verify_storage_proof(
        &l1_provider,
        &l2_provider,
        VerifyParams {
            address: args.address,
            keys: args.keys,
            batch_number: args.batch_number,
            l1_contract: args.l1_contract,
            bridgehub: args.bridgehub,
            commit_timeout,
        },
    )
    .await?;

    tracing::info!(
        computed = %result.computed_batch_hash,
        on_chain = %result.on_chain_batch_hash,
        "batch hash verified"
    );

    for (key, value) in &result.storage_values {
        match value {
            Some(v) => tracing::info!(key = %key, value = %v, "storage slot"),
            None => tracing::info!(key = %key, "storage slot (empty)"),
        }
    }

    tracing::info!("proof verified successfully");
    Ok(())
}
