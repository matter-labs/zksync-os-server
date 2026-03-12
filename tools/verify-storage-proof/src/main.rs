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

    /// Account address to prove storage for
    #[arg(long)]
    address: Address,

    /// Storage keys to verify (comma-separated)
    #[arg(long, value_delimiter = ',')]
    keys: Vec<B256>,

    /// L1 batch number
    #[arg(long)]
    batch_number: u64,

    /// Diamond proxy address on L1 (skips auto-discovery)
    #[arg(long)]
    l1_contract: Option<Address>,

    /// Bridgehub address on L1 (for auto-discovery of diamond proxy)
    #[arg(long)]
    bridgehub: Option<Address>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

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
        },
    )
    .await?;

    println!("Proof verified successfully against L1 batch commitment.");
    println!("  Batch number:        {}", args.batch_number);
    println!("  Storage commitment:  {}", result.storage_commitment);
    println!();
    println!("Storage values:");
    for (key, value) in &result.storage_values {
        match value {
            Some(v) => println!("  {key} => {v}"),
            None => println!("  {key} => (empty slot)"),
        }
    }

    Ok(())
}
