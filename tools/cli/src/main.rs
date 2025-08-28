use clap::{Parser, Subcommand};

use crate::{
    block::{Block, BlockMetadata},
    tx::{ZkOSTx, ZkOsReceipt, ZkOsTxMeta},
};

mod block;
mod rlp;
mod tx;

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    #[arg(short, long)]
    db_path: String,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Shows All the rows from current Database (must point as specific one).
    AllRows {},

    /// Displays basic information about the database.
    Info {},

    /// Displays information about the block.
    Block { block_number: u64 },

    /// Displays information about the transaction.
    Tx { hash: String },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let db_path = cli.db_path;

    match cli.command {
        Command::AllRows {} => {
            all_rows(db_path).await?;
        }
        Command::Info {} => {
            info_db(db_path).await?;
        }
        Command::Block { block_number } => show_block(&db_path, block_number, true)?,
        Command::Tx { hash } => show_tx(&db_path, &hash)?,
    }
    Ok(())
}

async fn all_rows(db_path: String) -> anyhow::Result<()> {
    println!("Showing status for database at: {db_path}");

    let path = std::path::Path::new(&db_path);
    let options = rocksdb::Options::default();

    let cf_names = match rocksdb::DB::list_cf(&options, path) {
        Ok(names) => names,
        Err(_) => vec!["default".to_string()], // fallback if listing fails
    };

    println!("Found CFs: {cf_names:?}");

    match rocksdb::DB::open_cf_for_read_only(&options, path, &cf_names, false) {
        Ok(db) => {
            println!("Opened RocksDB (read-only) at {db_path}");

            for cf_name in &cf_names {
                let cf = db
                    .cf_handle(cf_name)
                    .ok_or_else(|| anyhow::anyhow!("CF handle missing: {cf_name}"))?;
                println!("=== Column Family: {cf_name} ===");

                let mut count = 0usize;
                for (key, value) in db.iterator_cf(cf, rocksdb::IteratorMode::Start).flatten() {
                    println!("---");
                    println!("key (hex):   {}", hex::encode(&key));
                    println!("value (hex): {}", hex::encode(&value));
                    println!("key (utf8):  {}", String::from_utf8_lossy(&key));
                    println!("value (utf8): {}", String::from_utf8_lossy(&value));
                    count += 1;
                }
                println!("Total entries: {count}");
            }
        }
        Err(e) => {
            eprintln!("Failed to open RocksDB in read-only mode: {e}");
        }
    };
    Ok(())
}

async fn info_db(db_path: String) -> anyhow::Result<()> {
    println!("Displaying info for database at: {db_path}");

    // We'll access following things:

    let latest_block_info = latest_blocks(&db_path)?;

    // print latest block info in a nice format
    println!("Latest Block Info:");
    println!("  State: {}", latest_block_info.state);
    println!("  WAL: {}", latest_block_info.wal);
    println!("  Preimage: {}", latest_block_info.preimage);
    println!("  Repository: {}", latest_block_info.repository);

    if latest_block_info.wal < latest_block_info.repository {
        println!(
            "Warning: WAL is behind the repository. This may indicate an issue with the block processing."
        );
    }

    // Assume that 'wal' has the most recent info.
    let latest_block = latest_block_info.wal;

    // show last 5 blocks (if they are larger than state)

    println!("=== Last 5 blocks ===");

    for i in 0..=5 {
        let block_id = latest_block.saturating_sub(i);
        if block_id > latest_block_info.state {
            show_block(&db_path, block_id, false)?;
        }
    }

    Ok(())
}

fn read_from_rocksdb(
    db_path: &str,
    column_family: &str,
    key: &[u8],
) -> anyhow::Result<Option<Vec<u8>>> {
    let path = std::path::Path::new(db_path);
    let options = rocksdb::Options::default();
    let cf_names = [&column_family.to_string()];

    match rocksdb::DB::open_cf_for_read_only(&options, path, cf_names, false) {
        Ok(db) => {
            let cf = db
                .cf_handle(column_family)
                .ok_or_else(|| anyhow::anyhow!("Column family not found: {}", column_family))?;

            match db.get_cf(cf, key) {
                Ok(value) => Ok(value),
                Err(e) => Err(anyhow::anyhow!("Failed to read from RocksDB: {}", e)),
            }
        }
        Err(e) => Err(anyhow::anyhow!("Failed to open RocksDB: {}", e)),
    }
}

struct LatestBlocks {
    pub state: u64,
    pub wal: u64,
    pub preimage: u64,
    pub repository: u64,
}

fn latest_blocks(db_path: &str) -> anyhow::Result<LatestBlocks> {
    let path = std::path::Path::new(&db_path);

    let block = read_from_rocksdb(path.join("state").to_str().unwrap(), "meta", b"base_block")?
        .expect("last block info missing");

    let last_block_number = u64::from_be_bytes(block.as_slice().try_into().unwrap());

    let last_wal_entry = read_from_rocksdb(
        path.join("block_replay_wal").to_str().unwrap(),
        "latest",
        b"latest_block",
    )?
    .expect("last WAL entry missing");

    let last_wal_number = u64::from_be_bytes(last_wal_entry.as_slice().try_into().unwrap());

    // latest block from preimages
    let last_preimage_entry =
        read_from_rocksdb(path.join("preimages").to_str().unwrap(), "meta", b"block")?
            .expect("last preimage entry missing");

    let last_preimage_number =
        u64::from_be_bytes(last_preimage_entry.as_slice().try_into().unwrap());

    // latest block from repository
    let last_repo_entry = read_from_rocksdb(
        path.join("repository").to_str().unwrap(),
        "meta",
        b"block_number",
    )?
    .expect("last repository entry missing");

    let last_repo_number = u64::from_be_bytes(last_repo_entry.as_slice().try_into().unwrap());

    Ok(LatestBlocks {
        state: last_block_number,
        wal: last_wal_number,
        preimage: last_preimage_number,
        repository: last_repo_number,
    })
}

struct WALBlockInfo {
    pub hash: String,
    // Last L1 transaction id that was processed in this block.
    pub last_l1_tx_id: u64,

    pub l2_txs: usize,
    pub node_version: String,
    pub context: BlockMetadata,
}

fn wal_block_info(db_path: &str, block_number: u64) -> anyhow::Result<WALBlockInfo> {
    let path = std::path::Path::new(&db_path);

    let block_hash = read_from_rocksdb(
        path.join("block_replay_wal").to_str().unwrap(),
        "block_output_hash",
        &block_number.to_be_bytes(),
    )?
    .expect("block hash missing");

    let last_l1 = read_from_rocksdb(
        path.join("block_replay_wal").to_str().unwrap(),
        "last_processed_l1_tx_id",
        &block_number.to_be_bytes(),
    )?
    .expect("last L1 tx id missing");
    let last_l1 = vec_to_u64_be(&last_l1);

    let txs = read_from_rocksdb(
        path.join("block_replay_wal").to_str().unwrap(),
        "txs",
        &block_number.to_be_bytes(),
    )?
    .expect("txs missing");

    // node version
    let node_version = read_from_rocksdb(
        path.join("block_replay_wal").to_str().unwrap(),
        "node_version",
        &block_number.to_be_bytes(),
    )?
    .expect("node version missing");

    let (txs, _) =
        bincode::serde::decode_from_slice::<Vec<Vec<u8>>, _>(&txs, bincode::config::standard())?;

    let context = read_from_rocksdb(
        path.join("block_replay_wal").to_str().unwrap(),
        "context",
        &block_number.to_be_bytes(),
    )?
    .expect("context missing");

    let (context, _) = bincode::serde::decode_from_slice::<BlockMetadata, _>(
        &context,
        bincode::config::standard(),
    )?;

    // Stuff left:
    // - proofs table

    let block_info = WALBlockInfo {
        hash: hex::encode(block_hash),
        last_l1_tx_id: last_l1,
        l2_txs: txs.len(),
        node_version: String::from_utf8(node_version).expect("invalid UTF-8"),
        context,
    };

    Ok(block_info)
}

struct RepositoryBlockInfo {
    pub hash: String,
    pub block_data: Block,
}

fn repository_block_info(db_path: &str, block_number: u64) -> anyhow::Result<RepositoryBlockInfo> {
    let path = std::path::Path::new(&db_path);

    let block_hash = read_from_rocksdb(
        path.join("repository").to_str().unwrap(),
        "block_number_to_hash",
        &block_number.to_be_bytes(),
    )?
    .expect("block hash missing");

    let block_payload = read_from_rocksdb(
        path.join("repository").to_str().unwrap(),
        "block_data",
        &block_hash,
    )?
    .expect("block data missing");

    let block_rlp = rlp::decode(&block_payload)
        .map_err(|e| anyhow::anyhow!("Failed to decode block RLP: {:?}", e))?;

    let block_data = crate::block::Block::new_from_rlp(&block_rlp);

    let block_info = RepositoryBlockInfo {
        hash: hex::encode(block_hash),
        block_data,
    };

    Ok(block_info)
}

fn proof_info(db_path: &str, block_number: u64) -> anyhow::Result<bool> {
    let path = std::path::Path::new(&db_path);

    let proof = read_from_rocksdb(
        path.join("proofs").to_str().unwrap(),
        "proofs",
        &block_number.to_be_bytes(),
    )?;

    Ok(proof.is_some())
}

fn vec_to_u64_be(bytes: &[u8]) -> u64 {
    let mut out = 0u64;
    for b in bytes {
        out = (out << 8) | (*b as u64);
    }
    out
}

fn show_block(db_path: &str, block_number: u64, show_transactions: bool) -> anyhow::Result<()> {
    println!("==== Block {block_number}");
    let wal_block_info = wal_block_info(db_path, block_number)?;

    let label_width = 18usize;
    println!("  {:<label_width$} {}", "Output Hash:", wal_block_info.hash,);
    println!(
        "  {:<label_width$} {}",
        "Last l1 tx:", wal_block_info.last_l1_tx_id,
    );
    println!("  {:<label_width$} {}", "L2 txs:", wal_block_info.l2_txs,);

    println!(
        "  {:<label_width$} {}",
        "Node version:", wal_block_info.node_version,
    );
    println!(
        "  {:<label_width$} {:21}",
        "Context:", wal_block_info.context,
    );

    let repository_block_info = repository_block_info(db_path, block_number)?;
    println!(
        "  {:<label_width$} {}",
        "Block Hash:", repository_block_info.hash,
    );
    println!(
        "  {:<label_width$} {:21}",
        "Block data:", repository_block_info.block_data,
    );

    let proof = proof_info(db_path, block_number)?;
    println!("  {:<label_width$} {}", "Proof exists:", proof);

    if show_transactions {
        println!("  Transactions:");
        for tx in &repository_block_info.block_data.transactions {
            println!("    - {tx}");
        }
    }
    Ok(())
}

fn show_tx(db_path: &str, hash: &str) -> anyhow::Result<()> {
    println!("==== Transaction {hash}");
    let path = std::path::Path::new(&db_path);

    let hash = hex::decode(hash)?;

    let tx_data = read_from_rocksdb(path.join("repository").to_str().unwrap(), "tx", &hash)?
        .expect("Transaction data not found");

    let tx = ZkOSTx::from_bytes(&tx_data).expect("Failed to decode transaction");

    println!("{tx}");

    let receipt_data = read_from_rocksdb(
        path.join("repository").to_str().unwrap(),
        "tx_receipt",
        &hash,
    )?;

    if let Some(receipt_data) = receipt_data {
        let receipt = ZkOsReceipt::from_bytes(&receipt_data).unwrap();
        println!("{receipt}");
    } else {
        println!("  Receipt data: Not found");
    }

    let tx_meta = read_from_rocksdb(path.join("repository").to_str().unwrap(), "tx_meta", &hash)?;

    if let Some(tx_meta) = tx_meta {
        let tx_meta = ZkOsTxMeta::from_bytes(&tx_meta).unwrap();
        println!("{tx_meta}");
    } else {
        println!("  Transaction Meta: Not found");
    }

    Ok(())
}
