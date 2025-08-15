use clap::{Parser, Subcommand};
use std::fmt::Write;

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
    /// Shows the current status.
    Show {},
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let db_path = cli.db_path;

    println!("Hello {}", db_path);

    match cli.command {
        Command::Show {} => {
            // Here you would implement the logic to show the status.
            show_db(db_path).await;
        }
    }
    Ok(())
}

async fn show_db(db_path: String) {
    println!("Showing status for database at: {}", db_path);

    let path = std::path::Path::new(&db_path);
    fn to_hex(bytes: &[u8]) -> String {
        let mut s = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            write!(&mut s, "{:02x}", b).ok();
        }
        s
    }
    let options = rocksdb::Options::default();

    match rocksdb::DB::open_for_read_only(&options, path, false) {
        Ok(db) => {
            println!("Opened RocksDB (read-only) at {}", db_path);
            let mut count = 0usize;
            for result in db.iterator(rocksdb::IteratorMode::Start) {
                if let Ok((key, value)) = result {
                    println!("---");
                    println!("key (hex):   {}", to_hex(&key));
                    println!("value (hex): {}", to_hex(&value));
                    println!("key (utf8):  {}", String::from_utf8_lossy(&key));
                    println!("value (utf8): {}", String::from_utf8_lossy(&value));
                    count += 1;
                }
            }
            println!("Total entries: {}", count);
        }
        Err(e) => {
            eprintln!("Failed to open RocksDB in read-only mode: {}", e);
        }
    }
}
