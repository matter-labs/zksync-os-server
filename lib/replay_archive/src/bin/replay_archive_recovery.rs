use clap::{Parser, Subcommand};
use std::path::PathBuf;
use zksync_os_replay_archive::{
    FileSystemReplayArchiveReader, decrypt_downloaded_replay_archive_objects,
    download_all_replay_archive_objects, recover_replay_records_to_rocksdb,
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
    /// Decrypt previously downloaded replay archive objects to local disk.
    DecryptDownloaded {
        /// Local folder produced by the download command.
        #[arg(long)]
        input_root: PathBuf,
        /// Local folder where decrypted objects should be written.
        #[arg(long)]
        output_root: PathBuf,
        /// age identity file containing AGE-SECRET-KEY.
        #[arg(long)]
        identity_file: PathBuf,
    },
    /// Rebuild node replay RocksDB from downloaded plaintext replay records.
    RecoverRocksdb {
        /// Local folder containing plaintext replay records.
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
            let downloaded = download_all_replay_archive_objects(&reader, output_root).await?;
            println!("Downloaded {downloaded} replay archive objects");
        }
        Command::DecryptDownloaded {
            input_root,
            output_root,
            identity_file,
        } => {
            let decrypted =
                decrypt_downloaded_replay_archive_objects(input_root, output_root, identity_file)
                    .await?;
            println!("Decrypted {decrypted} replay archive objects");
        }
        Command::RecoverRocksdb {
            input_root,
            replay_db_path,
            anchor_block_number,
            anchor_block_hash,
        } => {
            let recovered = recover_replay_records_to_rocksdb(
                input_root,
                replay_db_path,
                anchor_block_number,
                anchor_block_hash,
            )
            .await?;
            println!("Recovered {recovered} replay records to RocksDB");
        }
    }

    Ok(())
}
