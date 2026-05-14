use clap::{Parser, Subcommand};
use std::path::PathBuf;
use zksync_os_replay_archive::{
    FileSystemReplayArchiveReader, download_all_replay_archive_objects, parse_age_x25519_identity,
    read_age_x25519_identity, recover_replay_records_to_rocksdb_with_optional_decryption,
};

#[derive(Debug, Parser)]
#[command(about = "Replay archive recovery utilities")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Download all archived replay record objects to local disk.
    Download {
        /// Root folder of the replay archive storage.
        #[arg(long)]
        archive_root: PathBuf,
        /// Local folder where downloaded objects should be written.
        #[arg(long)]
        output_root: PathBuf,
    },
    /// Rebuild node replay RocksDB from downloaded replay records.
    RecoverRocksdb {
        /// Local folder containing downloaded replay records.
        #[arg(long)]
        input_root: PathBuf,
        /// Output RocksDB path for block replay WAL.
        #[arg(long)]
        replay_db_path: PathBuf,
        /// Anchor block number to recover from.
        #[arg(long)]
        anchor_block_number: u64,
        /// Canonical anchor block hash.
        #[arg(long)]
        anchor_block_hash: alloy::primitives::BlockHash,
        /// age identity file containing AGE-SECRET-KEY. If provided, records are decrypted in memory.
        #[arg(long, conflicts_with = "age_secret_key")]
        identity_file: Option<PathBuf>,
        /// age AGE-SECRET-KEY value. If provided, records are decrypted in memory.
        #[arg(
            long,
            env = "REPLAY_ARCHIVE_AGE_SECRET_KEY",
            hide_env_values = true,
            conflicts_with = "identity_file"
        )]
        age_secret_key: Option<String>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Download {
            archive_root,
            output_root,
        } => {
            let reader = FileSystemReplayArchiveReader::new(archive_root);
            let downloaded = download_all_replay_archive_objects(&reader, &output_root).await?;
            println!("Downloaded {downloaded} replay archive objects");
        }
        Command::RecoverRocksdb {
            input_root,
            replay_db_path,
            anchor_block_number,
            anchor_block_hash,
            identity_file,
            age_secret_key,
        } => {
            let identity = if let Some(age_secret_key) = age_secret_key {
                Some(parse_age_x25519_identity(&age_secret_key)?)
            } else if let Some(identity_file) = identity_file {
                Some(read_age_x25519_identity(&identity_file).await?)
            } else {
                None
            };
            let recovered = recover_replay_records_to_rocksdb_with_optional_decryption(
                &input_root,
                &replay_db_path,
                anchor_block_number,
                anchor_block_hash,
                identity,
            )
            .await?;
            println!("Recovered {recovered} replay records to RocksDB");
        }
    }

    Ok(())
}
