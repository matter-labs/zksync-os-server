//! Scratch tool: decrypt one KMS-encrypted replay archive record and print a slice of its JSON.
//! Usage: cargo run -p zksync_os_replay_archive --example dump_record -- <file> <kms_key_version> [byte_offset]

use zksync_os_replay_archive::{
    ArchiveIdentity, GcpKmsAuthMode, GcpKmsClient, GcpKmsConfig, GcpKmsIdentity,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("record file path");
    let key_version = args.next().expect("kms key version");
    let offset: usize = args.next().map(|s| s.parse().unwrap()).unwrap_or(0);

    let client = GcpKmsClient::new(&GcpKmsConfig {
        key_version,
        auth_mode: GcpKmsAuthMode::Authenticated,
    })
    .await?;
    let identity = ArchiveIdentity::GcpKms(GcpKmsIdentity::new(client));
    let bytes = std::fs::read(&path)?;
    let json =
        tokio::task::spawn_blocking(move || age::decrypt(&identity, bytes.as_slice())).await??;

    let s = String::from_utf8_lossy(&json);
    eprintln!("total len: {}", s.len());
    if offset == 0 {
        println!("{}", &s[..s.len().min(2000)]);
    } else {
        let start = offset.saturating_sub(800);
        let end = (offset + 400).min(s.len());
        println!("{}", &s[start..end]);
    }

    match serde_json::from_slice::<zksync_os_storage_api::ReplayRecord>(&json) {
        Ok(record) => eprintln!(
            "decode OK: block #{}, {} transactions, node_version {}",
            record.block_context.block_number,
            record.transactions.len(),
            record.node_version
        ),
        Err(err) => eprintln!("decode FAILED: {err}"),
    }
    Ok(())
}
