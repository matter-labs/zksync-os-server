use crate::{ReplayArchiveKey, ReplayArchiveStorageReader, format_block_hash};
use alloy::primitives::{BlockHash, BlockNumber, Sealed};
use anyhow::Context as _;
use futures::StreamExt as _;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use zksync_os_storage::db::BlockReplayStorage;
use zksync_os_storage_api::{ReplayRecord, WriteReplay};

/// Downloads every archive object into a local layout grouped by block number and block hash.
///
/// The output layout is:
///
/// ```text
/// <output_root>/<block_number>/<block_hash>/<session>
/// ```
///
/// This keeps all session copies for the same replay record next to each other.
pub async fn download_all_replay_archive_objects<Reader>(
    reader: &Reader,
    output_root: &Path,
) -> anyhow::Result<usize>
where
    Reader: ReplayArchiveStorageReader + Sync,
{
    tracing::info!(
        output_root = %output_root.display(),
        "Starting replay archive object download"
    );
    let mut objects = reader.list_objects().await;
    let mut downloaded = 0;

    while let Some(object) = objects.next().await {
        let object = object?;
        write_downloaded_object(output_root, &object.key, object.bytes).await?;
        downloaded += 1;
        log_recovery_progress(downloaded, || {
            tracing::info!(downloaded, "Downloaded replay archive objects");
        });
    }

    tracing::info!(downloaded, "Finished replay archive object download");
    Ok(downloaded)
}

/// Rebuilds node replay RocksDB from downloaded plaintext replay records.
///
/// Starting from `anchor_block_number` and `anchor_block_hash`, this walks backwards through the
/// previous block hash recorded inside every replay record, verifies that all session copies for
/// the same `(block_number, block_hash)` are equal, and writes the recovered canonical chain into
/// `replay_db_path` using the node replay storage format.
pub async fn recover_replay_records_to_rocksdb(
    input_root: &Path,
    replay_db_path: &Path,
    anchor_block_number: BlockNumber,
    anchor_block_hash: BlockHash,
) -> anyhow::Result<usize> {
    recover_replay_records_to_rocksdb_with_optional_decryption(
        input_root,
        replay_db_path,
        anchor_block_number,
        anchor_block_hash,
        None,
    )
    .await
}

/// Rebuilds node replay RocksDB from downloaded replay records.
///
/// If `identity` is provided, every downloaded object is decrypted in memory before replay record
/// decoding. No decrypted archive objects are written to disk.
pub async fn recover_replay_records_to_rocksdb_with_optional_decryption(
    input_root: &Path,
    replay_db_path: &Path,
    anchor_block_number: BlockNumber,
    anchor_block_hash: BlockHash,
    identity: Option<age::x25519::Identity>,
) -> anyhow::Result<usize> {
    tracing::info!(
        input_root = %input_root.display(),
        replay_db_path = %replay_db_path.display(),
        anchor_block_number,
        %anchor_block_hash,
        "Starting replay archive RocksDB recovery"
    );
    if let Some(identity) = &identity {
        tracing::info!(
            "Replay archive RocksDB recovery will decrypt objects in memory, public key: {}",
            identity.to_public(),
        );
    }
    let decoder = ReplayRecordDecoder { identity };

    let mut canonical_chain = Vec::new();
    let mut block_number = anchor_block_number;
    let mut block_hash = anchor_block_hash;

    tracing::info!(
        anchor_block_number,
        %anchor_block_hash,
        "Walking canonical replay archive chain from anchor"
    );
    loop {
        let replay_record =
            read_verified_replay_record(input_root, block_number, block_hash, &decoder)
                .await
                .with_context(|| {
                    format!("failed to recover replay record #{block_number}, {block_hash}")
                })?;
        anyhow::ensure!(
            replay_record.block_context.block_number == block_number,
            "replay record path block number {block_number} does not match record block number {}",
            replay_record.block_context.block_number
        );
        let previous_block_hash = replay_record.block_context.block_hashes.0[255]
            .to_be_bytes()
            .into();

        canonical_chain.push((block_number, block_hash));
        log_recovery_progress(canonical_chain.len(), || {
            tracing::info!(
                records = canonical_chain.len(),
                block_number,
                %block_hash,
                "Walked canonical replay archive records"
            );
        });
        if block_number == 0 {
            break;
        }
        block_number -= 1;
        block_hash = previous_block_hash;
    }

    tracing::info!(
        records = canonical_chain.len(),
        "Finished canonical replay archive chain walk"
    );
    canonical_chain.reverse();
    let recovered_count = canonical_chain.len();
    let replay_storage = BlockReplayStorage::new_without_genesis(replay_db_path);
    tracing::info!(
        recovered_count,
        replay_db_path = %replay_db_path.display(),
        "Writing recovered replay records to RocksDB"
    );
    for (block_number, block_hash) in canonical_chain {
        let replay_record =
            read_verified_replay_record(input_root, block_number, block_hash, &decoder)
                .await
                .with_context(|| {
                    format!(
                        "failed to read replay record #{block_number}, {block_hash} for writing"
                    )
                })?;
        anyhow::ensure!(
            replay_storage
                .write(Sealed::new_unchecked(replay_record, block_hash), false)
                .await?,
            "replay record #{block_number} already exists in recovered RocksDB"
        );
        log_recovery_progress((block_number + 1) as usize, || {
            tracing::info!(
                block_number,
                %block_hash,
                "Wrote recovered replay record to RocksDB"
            );
        });
    }

    tracing::info!(
        recovered_count,
        replay_db_path = %replay_db_path.display(),
        "Finished replay archive RocksDB recovery"
    );
    Ok(recovered_count)
}

fn log_recovery_progress(count: usize, log: impl FnOnce()) {
    if count <= 10 || count.is_power_of_two() || count.is_multiple_of(1_000) {
        log();
    }
}

async fn write_downloaded_object(
    output_root: &Path,
    key: &ReplayArchiveKey,
    object: Vec<u8>,
) -> anyhow::Result<()> {
    let output_path = output_root
        .join(key.block_number.to_string())
        .join(format_block_hash(key.block_hash))
        .join(key.session.folder_name());

    let parent = output_path
        .parent()
        .expect("downloaded replay archive object path must have a parent");
    tokio::fs::create_dir_all(parent).await.with_context(|| {
        format!(
            "failed to create replay archive recovery directory {}",
            parent.display()
        )
    })?;

    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&output_path)
        .await
        .with_context(|| {
            format!(
                "failed to create replay archive recovery object {}",
                output_path.display()
            )
        })?;
    file.write_all(&object).await.with_context(|| {
        format!(
            "failed to write replay archive recovery object {}",
            output_path.display()
        )
    })?;
    file.flush().await.with_context(|| {
        format!(
            "failed to flush replay archive recovery object {}",
            output_path.display()
        )
    })?;
    Ok(())
}

async fn read_verified_replay_record(
    input_root: &Path,
    block_number: BlockNumber,
    block_hash: BlockHash,
    decoder: &ReplayRecordDecoder,
) -> anyhow::Result<ReplayRecord> {
    let replay_record_dir = input_root
        .join(block_number.to_string())
        .join(format_block_hash(block_hash));
    let mut entries = tokio::fs::read_dir(&replay_record_dir)
        .await
        .with_context(|| {
            format!(
                "missing replay archive records for block #{block_number}, {block_hash} at {}",
                replay_record_dir.display()
            )
        })?;

    let mut canonical_record: Option<ReplayRecord> = None;
    let mut canonical_path: Option<PathBuf> = None;
    let mut records_count = 0;
    while let Some(entry) = entries.next_entry().await.with_context(|| {
        format!(
            "failed to read replay archive records directory {}",
            replay_record_dir.display()
        )
    })? {
        let file_type = entry.file_type().await.with_context(|| {
            format!(
                "failed to read replay archive record file type {}",
                entry.path().display()
            )
        })?;
        if !file_type.is_file() {
            continue;
        }

        let record_bytes = tokio::fs::read(entry.path()).await.with_context(|| {
            format!(
                "failed to read replay archive record file {}",
                entry.path().display()
            )
        })?;
        let record = decoder.decode(record_bytes, &entry.path())?;
        if let Some(canonical_record) = &canonical_record {
            anyhow::ensure!(
                canonical_record == &record,
                "Replay archive record differs between sessions for block #{block_number}, {block_hash}. Paths: {}, {}",
                entry.path().display(),
                canonical_path.unwrap().display(),
            );
        } else {
            canonical_record = Some(record);
            canonical_path = Some(entry.path());
        }
        records_count += 1;
    }

    anyhow::ensure!(
        records_count > 0,
        "no replay archive record files found for block #{block_number}, {block_hash}"
    );
    canonical_record.context("replay archive record count was non-zero but no record was loaded")
}

struct ReplayRecordDecoder {
    identity: Option<age::x25519::Identity>,
}

impl ReplayRecordDecoder {
    fn decode(&self, mut record_bytes: Vec<u8>, path: &Path) -> anyhow::Result<ReplayRecord> {
        if let Some(identity) = &self.identity {
            record_bytes = age::decrypt(identity, record_bytes.as_slice()).with_context(|| {
                format!(
                    "failed to decrypt replay archive record file {}",
                    path.display()
                )
            })?;
        }
        serde_json::from_slice(&record_bytes).with_context(|| {
            format!(
                "failed to decode replay archive record file {}",
                path.display()
            )
        })
    }
}

pub async fn read_age_x25519_identity(
    identity_file: &Path,
) -> anyhow::Result<age::x25519::Identity> {
    let file = tokio::fs::File::open(identity_file)
        .await
        .with_context(|| {
            format!(
                "failed to read age identity file {}",
                identity_file.display()
            )
        })?;
    let mut lines = BufReader::new(file).lines();
    while let Some(line) = lines.next_line().await.with_context(|| {
        format!(
            "failed to read age identity file {}",
            identity_file.display()
        )
    })? {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        return parse_age_x25519_identity(line);
    }
    anyhow::bail!(
        "age identity file {} does not contain an AGE-SECRET-KEY identity",
        identity_file.display()
    );
}

pub fn parse_age_x25519_identity(identity: &str) -> anyhow::Result<age::x25519::Identity> {
    identity
        .parse()
        .map_err(|err| anyhow::anyhow!("failed to parse age X25519 identity: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        FileSystemReplayArchiveReader, FileSystemReplayArchiveStorage, ReplayArchiveSession,
        ReplayArchiveStorage,
    };
    use age::secrecy::ExposeSecret as _;
    use alloy::primitives::{B256, U256};
    use zksync_os_storage::db::BlockReplayStorage;
    use zksync_os_storage_api::ReadReplay;

    #[tokio::test]
    async fn filesystem_reader_downloads_objects_grouped_by_block_and_hash() {
        let archive_root = tempfile::tempdir().unwrap();
        let output_root = tempfile::tempdir().unwrap();
        let block_hash = B256::with_last_byte(1);

        let first_session = ReplayArchiveSession::new(42, "node-a").unwrap();
        let first_storage =
            FileSystemReplayArchiveStorage::init(archive_root.path().to_path_buf(), first_session)
                .await
                .unwrap();
        first_storage
            .append_object(7, block_hash, b"first".to_vec())
            .await
            .unwrap();

        let second_session = ReplayArchiveSession::new(43, "node-b").unwrap();
        let second_storage =
            FileSystemReplayArchiveStorage::init(archive_root.path().to_path_buf(), second_session)
                .await
                .unwrap();
        second_storage
            .append_object(7, block_hash, b"second".to_vec())
            .await
            .unwrap();

        let reader = FileSystemReplayArchiveReader::new(archive_root.path().to_path_buf());
        let downloaded = download_all_replay_archive_objects(&reader, output_root.path())
            .await
            .unwrap();

        assert_eq!(downloaded, 2);
        let block_hash = crate::format_block_hash(block_hash);
        assert_eq!(
            tokio::fs::read(
                output_root
                    .path()
                    .join("7")
                    .join(&block_hash)
                    .join("42-node-a")
            )
            .await
            .unwrap(),
            b"first"
        );
        assert_eq!(
            tokio::fs::read(
                output_root
                    .path()
                    .join("7")
                    .join(&block_hash)
                    .join("43-node-b")
            )
            .await
            .unwrap(),
            b"second"
        );
    }

    #[tokio::test]
    async fn recover_records_to_rocksdb_walks_from_anchor_and_writes_node_format() {
        let input_root = tempfile::tempdir().unwrap();
        let replay_db = tempfile::tempdir().unwrap();
        let genesis_hash = B256::with_last_byte(1);
        let block_hash = B256::with_last_byte(2);
        let genesis_record = test_replay_record(0, B256::ZERO);
        let block_record = test_replay_record(1, genesis_hash);

        write_downloaded_replay_record(
            input_root.path(),
            0,
            genesis_hash,
            "42-node-a",
            &genesis_record,
        )
        .await;
        write_downloaded_replay_record(
            input_root.path(),
            1,
            block_hash,
            "42-node-a",
            &block_record,
        )
        .await;
        write_downloaded_replay_record(
            input_root.path(),
            1,
            block_hash,
            "43-node-b",
            &block_record,
        )
        .await;

        let recovered =
            recover_replay_records_to_rocksdb(input_root.path(), replay_db.path(), 1, block_hash)
                .await
                .unwrap();

        let replay_storage = BlockReplayStorage::new_without_genesis(replay_db.path());
        assert_eq!(recovered, 2);
        assert_eq!(replay_storage.latest_record(), 1);
        assert_eq!(replay_storage.get_replay_record(0).unwrap(), genesis_record);
        assert_eq!(replay_storage.get_replay_record(1).unwrap(), block_record);
    }

    #[tokio::test]
    async fn recover_records_to_rocksdb_decrypts_records_in_memory() {
        let input_root = tempfile::tempdir().unwrap();
        let replay_db = tempfile::tempdir().unwrap();
        let identity_file = tempfile::NamedTempFile::new().unwrap();
        let identity = age::x25519::Identity::generate();
        let recipient = identity.to_public();
        let genesis_hash = B256::with_last_byte(1);
        let block_hash = B256::with_last_byte(2);
        let genesis_record = test_replay_record(0, B256::ZERO);
        let block_record = test_replay_record(1, genesis_hash);

        write_downloaded_encrypted_replay_record(
            input_root.path(),
            0,
            genesis_hash,
            "42-node-a",
            &genesis_record,
            &recipient,
        )
        .await;
        write_downloaded_encrypted_replay_record(
            input_root.path(),
            1,
            block_hash,
            "42-node-a",
            &block_record,
            &recipient,
        )
        .await;
        tokio::fs::write(
            identity_file.path(),
            format!(
                "# public key: {}\n{}\n",
                recipient,
                identity.to_string().expose_secret()
            ),
        )
        .await
        .unwrap();

        let identity = read_age_x25519_identity(identity_file.path())
            .await
            .unwrap();
        let recovered = recover_replay_records_to_rocksdb_with_optional_decryption(
            input_root.path(),
            replay_db.path(),
            1,
            block_hash,
            Some(identity),
        )
        .await
        .unwrap();

        let replay_storage = BlockReplayStorage::new_without_genesis(replay_db.path());
        assert_eq!(recovered, 2);
        assert_eq!(replay_storage.get_replay_record(0).unwrap(), genesis_record);
        assert_eq!(replay_storage.get_replay_record(1).unwrap(), block_record);
    }

    #[tokio::test]
    async fn recover_records_rejects_different_session_copies() {
        let input_root = tempfile::tempdir().unwrap();
        let replay_db = tempfile::tempdir().unwrap();
        let block_hash = B256::with_last_byte(1);
        let record = test_replay_record(0, B256::ZERO);
        let mut different_record = record.clone();
        different_record.block_output_hash = B256::with_last_byte(2);

        write_downloaded_replay_record(input_root.path(), 0, block_hash, "42-node-a", &record)
            .await;
        write_downloaded_replay_record(
            input_root.path(),
            0,
            block_hash,
            "43-node-b",
            &different_record,
        )
        .await;

        let err =
            recover_replay_records_to_rocksdb(input_root.path(), replay_db.path(), 0, block_hash)
                .await
                .unwrap_err();

        assert!(
            err.to_string()
                .contains("failed to recover replay record #0"),
            "{err:#}"
        );
        assert!(
            format!("{err:#}").contains("differs between sessions"),
            "{err:#}"
        );
    }

    async fn write_downloaded_replay_record(
        input_root: &Path,
        block_number: BlockNumber,
        block_hash: BlockHash,
        session: &str,
        record: &ReplayRecord,
    ) {
        let path = input_root
            .join(block_number.to_string())
            .join(format_block_hash(block_hash))
            .join(session);
        tokio::fs::create_dir_all(path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(path, serde_json::to_vec(record).unwrap())
            .await
            .unwrap();
    }

    async fn write_downloaded_encrypted_replay_record(
        input_root: &Path,
        block_number: BlockNumber,
        block_hash: BlockHash,
        session: &str,
        record: &ReplayRecord,
        recipient: &age::x25519::Recipient,
    ) {
        let path = input_root
            .join(block_number.to_string())
            .join(format_block_hash(block_hash))
            .join(session);
        tokio::fs::create_dir_all(path.parent().unwrap())
            .await
            .unwrap();
        let encrypted =
            age::encrypt(recipient, serde_json::to_vec(record).unwrap().as_slice()).unwrap();
        tokio::fs::write(path, encrypted).await.unwrap();
    }

    fn test_replay_record(
        block_number: BlockNumber,
        previous_block_hash: BlockHash,
    ) -> ReplayRecord {
        let mut block_context = zksync_os_interface::types::BlockContext {
            block_number,
            ..Default::default()
        };
        block_context.block_hashes.0[255] = U256::from_be_slice(previous_block_hash.as_slice());
        ReplayRecord {
            block_context,
            transactions: vec![],
            previous_block_timestamp: 0,
            node_version: "0.0.0".parse().unwrap(),
            protocol_version: "0.29.1".parse().unwrap(),
            block_output_hash: B256::ZERO,
            force_preimages: vec![],
            starting_cursors: Default::default(),
        }
    }
}
