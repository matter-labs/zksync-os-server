use alloy::primitives::{Address, B256};
use alloy::providers::ProviderBuilder;
use clap::Parser;
use zksync_os_verify_storage_proof::l1::{fetch_stored_batch_hash, resolve_diamond_proxy};
use zksync_os_verify_storage_proof::l2::fetch_proof;
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

    println!(
        "Connecting to L1 ({}) and L2 ({})...",
        args.l1_rpc, args.l2_rpc
    );
    let l1_provider = ProviderBuilder::new().connect(&args.l1_rpc).await?;
    let l2_provider = ProviderBuilder::new().connect(&args.l2_rpc).await?;

    // 1. Resolve diamond proxy
    println!("\n--- Step 1: Resolve diamond proxy ---");
    let diamond_proxy =
        resolve_diamond_proxy(&l1_provider, &l2_provider, args.l1_contract, args.bridgehub).await?;
    println!("  Diamond proxy: {diamond_proxy}");

    // 2. Fetch on-chain batch hash
    println!("\n--- Step 2: Fetch on-chain batch hash ---");
    let on_chain_hash =
        fetch_stored_batch_hash(&l1_provider, diamond_proxy, args.batch_number).await?;
    println!("  storedBatchHash({}) = {on_chain_hash}", args.batch_number);

    // 3. Fetch proof from L2
    println!("\n--- Step 3: Fetch storage proof from L2 ---");
    let proof = fetch_proof(
        &l2_provider,
        args.address,
        args.keys.clone(),
        args.batch_number,
    )
    .await?;

    let sc = &proof.state_commitment_preimage;
    println!("  Address:         {}", proof.address);
    println!("  Next free slot:  {}", sc.next_free_slot);
    println!("  Block number:    {}", sc.block_number);
    println!("  Block timestamp: {}", sc.last_block_timestamp);

    let l1v = &proof.l1_verification_data;
    println!("  L1 batch:        {}", l1v.batch_number);
    println!("  L1 txs:          {}", l1v.number_of_layer1_txs);
    println!("  Priority ops:    {}", l1v.priority_operations_hash);
    println!("  Commitment:      {}", l1v.commitment);

    for (i, slot_proof) in proof.storage_proofs.iter().enumerate() {
        use zksync_os_merkle_tree_api::flat::InnerStorageSlotProof;
        match &slot_proof.proof {
            InnerStorageSlotProof::Existing(entry) => {
                println!(
                    "  Slot {i}: key={} index={} value={} (existing)",
                    slot_proof.key.0, entry.index, entry.value
                );
            }
            InnerStorageSlotProof::NonExisting { .. } => {
                println!("  Slot {i}: key={} (non-existing)", slot_proof.key.0);
            }
        }
    }

    // 4. Verify
    println!("\n--- Step 4: Verify proof ---");
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

    let hashes_match = result.computed_batch_hash == result.on_chain_batch_hash;
    println!("  Computed batch hash: {}", result.computed_batch_hash);
    println!("  On-chain batch hash: {}", result.on_chain_batch_hash);
    println!("  Match: {hashes_match}");

    // 5. Print storage values
    println!("\n--- Storage values for {} ---", args.address);
    for (key, value) in &result.storage_values {
        match value {
            Some(v) => println!("  {key} => {v}"),
            None => println!("  {key} => (empty slot)"),
        }
    }

    println!("\nProof verified successfully.");
    Ok(())
}
