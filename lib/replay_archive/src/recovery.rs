#[cfg(feature = "gcp")]
use crate::kms::GcpKmsIdentity;
use crate::{ReplayArchiveKey, ReplayArchiveStorageReader, format_block_hash};
use age_core::format::{FileKey, Stanza};
use alloy::primitives::{BlockHash, BlockNumber, Sealed};
use anyhow::Context as _;
use futures::{StreamExt as _, TryStreamExt as _};
use std::collections::{HashMap, HashSet};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use zksync_os_storage::db::BlockReplayStorage;
use zksync_os_storage_api::{ReplayRecord, WriteReplay};

/// Default number of archive objects fetched concurrently during download.
pub const DEFAULT_DOWNLOAD_CONCURRENCY: NonZeroUsize = NonZeroUsize::new(32).unwrap();

/// Default number of replay records decoded concurrently during recovery.
pub const DEFAULT_DECRYPT_CONCURRENCY: NonZeroUsize = NonZeroUsize::new(32).unwrap();

/// Downloads every archive object into a local layout grouped by block number and block hash.
///
/// The output layout is:
///
/// ```text
/// <output_root>/<block_number>/<block_hash>
/// ```
///
/// Objects already present under `output_root` are skipped without re-downloading, so an
/// interrupted download can be restarted with the same arguments.
///
/// `download_concurrency` bounds how many objects are fetched from the archive at once.
pub async fn download_all_replay_archive_objects<Reader>(
    reader: &Reader,
    output_root: &Path,
    download_concurrency: NonZeroUsize,
) -> anyhow::Result<usize>
where
    Reader: ReplayArchiveStorageReader + Sync,
{
    tracing::info!(
        output_root = %output_root.display(),
        "Starting replay archive object download"
    );
    let existing = scan_existing_downloaded_objects(output_root).await?;
    if !existing.is_empty() {
        tracing::info!(
            existing = existing.len(),
            "Resuming download; objects already present locally are skipped"
        );
    }

    // Listing and downloading are interleaved page by page: downloads start right after the
    // first listing page instead of waiting for a full listing of a large archive.
    let mut page_token = None;
    let mut downloaded = 0;

    loop {
        let page = reader.list_keys_page(page_token.take()).await?;

        let mut objects = futures::stream::iter(
            page.keys
                .into_iter()
                .filter(|key| !existing.contains(key))
                .map(|key| async move {
                    let bytes = reader.fetch_object(&key).await?;
                    anyhow::Ok((key, bytes))
                }),
        )
        .buffer_unordered(download_concurrency.get());

        while let Some(object) = objects.next().await {
            let (key, bytes) = object?;
            write_downloaded_object(output_root, &key, bytes).await?;
            downloaded += 1;
            log_recovery_progress(downloaded, || {
                tracing::info!(downloaded, "Downloaded replay archive objects");
            });
        }

        match page.next_page_token {
            Some(token) => page_token = Some(token),
            None => break,
        }
    }

    tracing::info!(downloaded, "Finished replay archive object download");
    Ok(downloaded)
}

/// Rebuilds node replay RocksDB from downloaded plaintext replay records.
///
/// Starting from `anchor_block_number` and `anchor_block_hash`, this walks backwards through the
/// previous block hash recorded inside every replay record and writes the recovered canonical
/// chain into `replay_db_path` using the node replay storage format.
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
        DEFAULT_DECRYPT_CONCURRENCY,
    )
    .await
}

/// Rebuilds node replay RocksDB from downloaded replay records.
///
/// If `identity` is provided, every downloaded object is decrypted in memory before replay record
/// decoding. No decrypted archive objects are written to disk.
///
/// Records are decoded up to `decrypt_concurrency` blocks at a time. The canonical chain walk is
/// inherently sequential (the parent hash lives inside the decrypted record), so it decodes
/// windows of consecutive block numbers speculatively and then verifies linkage in memory. This
/// matters for identities whose file key unwrap is a network call, like GCP KMS.
pub async fn recover_replay_records_to_rocksdb_with_optional_decryption(
    input_root: &Path,
    replay_db_path: &Path,
    anchor_block_number: BlockNumber,
    anchor_block_hash: BlockHash,
    identity: Option<ArchiveIdentity>,
    decrypt_concurrency: NonZeroUsize,
) -> anyhow::Result<usize> {
    tracing::info!(
        input_root = %input_root.display(),
        replay_db_path = %replay_db_path.display(),
        anchor_block_number,
        %anchor_block_hash,
        "Starting replay archive RocksDB recovery"
    );
    match &identity {
        Some(ArchiveIdentity::X25519(identity)) => tracing::info!(
            "Replay archive RocksDB recovery will decrypt objects in memory, public key: {}",
            identity.to_public(),
        ),
        #[cfg(feature = "gcp")]
        Some(ArchiveIdentity::GcpKms(identity)) => tracing::info!(
            "Replay archive RocksDB recovery will decrypt objects in memory \
             via GCP KMS key version {}",
            identity.key_version(),
        ),
        None => {}
    }
    let decoder = Arc::new(ReplayRecordDecoder { identity });
    // Keep the decode pipeline saturated while bounding how many decoded records are held in
    // memory at once.
    let window_size = (decrypt_concurrency.get() as u64) * 2;

    // The chain id is shared by all blocks; take it from the anchor record.
    let chain_id =
        read_verified_replay_record(input_root, anchor_block_number, anchor_block_hash, &decoder)
            .await
            .with_context(|| {
                format!(
                    "failed to recover replay record #{anchor_block_number}, {anchor_block_hash}"
                )
            })?
            .block_context
            .chain_id;

    let mut canonical_chain = Vec::new();
    let mut window_top = anchor_block_number;
    let mut block_hash = anchor_block_hash;

    tracing::info!(
        anchor_block_number,
        %anchor_block_hash,
        "Walking canonical replay archive chain from anchor"
    );
    loop {
        let window_start = (window_top + 1).saturating_sub(window_size);
        let mut window: HashMap<BlockNumber, Vec<(BlockHash, anyhow::Result<ReplayRecord>)>> =
            futures::stream::iter(window_start..=window_top)
                .map(|block_number| {
                    let decoder = decoder.clone();
                    let input_root = input_root.to_path_buf();
                    async move {
                        let candidates =
                            read_candidate_records(&input_root, block_number, &decoder)
                                .await
                                .with_context(|| {
                                    format!("failed to recover replay record #{block_number}")
                                })?;
                        anyhow::Ok((block_number, candidates))
                    }
                })
                .buffer_unordered(decrypt_concurrency.get())
                .try_collect()
                .await?;

        for block_number in (window_start..=window_top).rev() {
            let candidates = window
                .remove(&block_number)
                .context("replay archive recovery window is missing a block")?;
            let mut canonical_candidate = None;
            for (hash, record) in candidates {
                if hash == block_hash {
                    canonical_candidate = Some(record);
                } else if let Err(err) = record {
                    // Only the canonical record must decode; a broken record from a
                    // reorged-out block is skipped.
                    tracing::warn!(
                        block_number,
                        non_canonical_block_hash = %hash,
                        error = format!("{err:#}"),
                        "Skipping non-canonical replay archive record that failed to decode"
                    );
                }
            }
            let replay_record = canonical_candidate
                .with_context(|| {
                    format!(
                        "missing replay archive records for block #{block_number}, {block_hash}"
                    )
                })?
                .with_context(|| format!("failed to recover replay record #{block_number}"))?;
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
            block_hash = previous_block_hash;
        }
        if window_start == 0 {
            break;
        }
        window_top = window_start - 1;
    }

    tracing::info!(
        records = canonical_chain.len(),
        "Finished canonical replay archive chain walk"
    );
    canonical_chain.reverse();
    let recovered_count = canonical_chain.len();
    let replay_storage = BlockReplayStorage::new_without_genesis(replay_db_path, chain_id);
    tracing::info!(
        recovered_count,
        replay_db_path = %replay_db_path.display(),
        "Writing recovered replay records to RocksDB"
    );
    // `buffered` keeps up to `decrypt_concurrency` records decoding ahead of the sequential
    // writes while preserving the genesis-upward order.
    let mut records = futures::stream::iter(canonical_chain)
        .map(|(block_number, block_hash)| {
            let decoder = decoder.clone();
            let input_root = input_root.to_path_buf();
            async move {
                let replay_record = read_verified_replay_record(
                    &input_root,
                    block_number,
                    block_hash,
                    &decoder,
                )
                .await
                .with_context(|| {
                    format!(
                        "failed to read replay record #{block_number}, {block_hash} for writing"
                    )
                })?;
                anyhow::Ok((block_number, block_hash, replay_record))
            }
        })
        .buffered(decrypt_concurrency.get());
    while let Some(record) = records.next().await {
        let (block_number, block_hash, replay_record) = record?;
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
    if count.is_multiple_of(50) {
        log();
    }
}

/// Scans an existing download output root for already-downloaded objects.
///
/// Entries that do not parse as `<block_number>/<block_hash>` are ignored, including `.partial`
/// files left behind by an interrupted write: they get re-downloaded and overwritten.
async fn scan_existing_downloaded_objects(
    output_root: &Path,
) -> anyhow::Result<HashSet<ReplayArchiveKey>> {
    let mut existing = HashSet::new();
    if !tokio::fs::try_exists(output_root).await? {
        return Ok(existing);
    }

    let mut block_entries = tokio::fs::read_dir(output_root).await.with_context(|| {
        format!(
            "failed to read replay archive recovery root {}",
            output_root.display()
        )
    })?;
    while let Some(block_entry) = block_entries.next_entry().await? {
        let Ok(block_number) = block_entry
            .file_name()
            .to_string_lossy()
            .parse::<BlockNumber>()
        else {
            continue;
        };
        if !block_entry.file_type().await?.is_dir() {
            continue;
        }

        let mut hash_entries = tokio::fs::read_dir(block_entry.path()).await?;
        while let Some(hash_entry) = hash_entries.next_entry().await? {
            let Ok(block_hash) = hash_entry
                .file_name()
                .to_string_lossy()
                .parse::<BlockHash>()
            else {
                continue;
            };
            if !hash_entry.file_type().await?.is_file() {
                continue;
            }
            existing.insert(ReplayArchiveKey::new(block_number, block_hash));
        }
    }

    Ok(existing)
}

async fn write_downloaded_object(
    output_root: &Path,
    key: &ReplayArchiveKey,
    object: Vec<u8>,
) -> anyhow::Result<()> {
    let output_path = output_root
        .join(key.block_number.to_string())
        .join(format_block_hash(key.block_hash));

    let parent = output_path
        .parent()
        .expect("downloaded replay archive object path must have a parent");
    tokio::fs::create_dir_all(parent).await.with_context(|| {
        format!(
            "failed to create replay archive recovery directory {}",
            parent.display()
        )
    })?;

    // Write to a temporary name and rename so that an object file only exists under its
    // final name once fully written; interrupted downloads leave `.partial` files that the
    // resume scan ignores and the next attempt overwrites.
    let partial_path = output_path.with_extension("partial");
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&partial_path)
        .await
        .with_context(|| {
            format!(
                "failed to create replay archive recovery object {}",
                partial_path.display()
            )
        })?;
    file.write_all(&object).await.with_context(|| {
        format!(
            "failed to write replay archive recovery object {}",
            partial_path.display()
        )
    })?;
    file.flush().await.with_context(|| {
        format!(
            "failed to flush replay archive recovery object {}",
            partial_path.display()
        )
    })?;
    drop(file);
    tokio::fs::rename(&partial_path, &output_path)
        .await
        .with_context(|| {
            format!(
                "failed to finalize replay archive recovery object {}",
                output_path.display()
            )
        })?;
    Ok(())
}

/// Reads and decodes all `(block_hash, record)` candidates present for a block number.
///
/// Decode failures are captured per candidate instead of failing the whole read: only the
/// candidate selected by the canonical chain walk must decode successfully, so a corrupt or
/// undecryptable record from a reorged-out block does not abort recovery.
async fn read_candidate_records(
    input_root: &Path,
    block_number: BlockNumber,
    decoder: &Arc<ReplayRecordDecoder>,
) -> anyhow::Result<Vec<(BlockHash, anyhow::Result<ReplayRecord>)>> {
    let block_dir = input_root.join(block_number.to_string());
    let mut entries = tokio::fs::read_dir(&block_dir).await.with_context(|| {
        format!(
            "missing replay archive records for block #{block_number} at {}",
            block_dir.display()
        )
    })?;

    let mut candidates = Vec::new();
    while let Some(entry) = entries.next_entry().await.with_context(|| {
        format!(
            "failed to read replay archive block directory {}",
            block_dir.display()
        )
    })? {
        let file_type = entry.file_type().await.with_context(|| {
            format!(
                "failed to read replay archive entry file type {}",
                entry.path().display()
            )
        })?;
        if !file_type.is_file() {
            continue;
        }
        let file_name = entry.file_name();
        let Some(block_hash) = file_name
            .to_str()
            .and_then(|name| name.parse::<BlockHash>().ok())
        else {
            if !file_name.to_string_lossy().ends_with(".partial") {
                tracing::warn!(
                    path = %entry.path().display(),
                    "Skipping replay archive entry that is not a block hash"
                );
            }
            continue;
        };
        let record =
            read_verified_replay_record(input_root, block_number, block_hash, decoder).await;
        candidates.push((block_hash, record));
    }
    anyhow::ensure!(
        !candidates.is_empty(),
        "no replay archive records found for block #{block_number}"
    );
    Ok(candidates)
}

async fn read_verified_replay_record(
    input_root: &Path,
    block_number: BlockNumber,
    block_hash: BlockHash,
    decoder: &Arc<ReplayRecordDecoder>,
) -> anyhow::Result<ReplayRecord> {
    let replay_record_path = input_root
        .join(block_number.to_string())
        .join(format_block_hash(block_hash));
    let record_bytes = tokio::fs::read(&replay_record_path)
        .await
        .with_context(|| {
            format!(
                "missing replay archive record for block #{block_number}, {block_hash} at {}",
                replay_record_path.display()
            )
        })?;
    decoder
        .decode_off_thread(record_bytes, replay_record_path)
        .await
}

/// age identity used for replay archive record decryption.
pub enum ArchiveIdentity {
    X25519(age::x25519::Identity),
    #[cfg(feature = "gcp")]
    GcpKms(GcpKmsIdentity),
}

impl age::Identity for ArchiveIdentity {
    fn unwrap_stanza(&self, stanza: &Stanza) -> Option<Result<FileKey, age::DecryptError>> {
        match self {
            Self::X25519(identity) => identity.unwrap_stanza(stanza),
            #[cfg(feature = "gcp")]
            Self::GcpKms(identity) => identity.unwrap_stanza(stanza),
        }
    }
}

struct ReplayRecordDecoder {
    identity: Option<ArchiveIdentity>,
}

impl ReplayRecordDecoder {
    /// Decodes on the blocking thread pool: a `GcpKms` identity blocks on a KMS call while
    /// unwrapping the file key, which must not happen on an async worker thread.
    async fn decode_off_thread(
        self: &Arc<Self>,
        record_bytes: Vec<u8>,
        path: PathBuf,
    ) -> anyhow::Result<ReplayRecord> {
        let decoder = self.clone();
        tokio::task::spawn_blocking(move || decoder.decode(record_bytes, &path))
            .await
            .context("replay archive record decode task panicked")?
    }

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
        FileSystemReplayArchiveReader, FileSystemReplayArchiveStorage, ReplayArchiveStorage,
    };
    use age::secrecy::ExposeSecret as _;
    use alloy::primitives::{B256, U256};
    use zksync_os_storage::db::BlockReplayStorage;
    use zksync_os_storage_api::ReadReplay;

    #[tokio::test]
    async fn restarted_download_skips_complete_objects_and_redownloads_partial_ones() {
        let archive_root = tempfile::tempdir().unwrap();
        let output_root = tempfile::tempdir().unwrap();
        let block_hash = B256::with_last_byte(1);

        let storage = FileSystemReplayArchiveStorage::init(
            archive_root.path().to_path_buf(),
            "node-a".to_owned(),
        )
        .await
        .unwrap();
        storage
            .put_object_if_absent(7, block_hash, b"first".to_vec())
            .await
            .unwrap();
        storage
            .put_object_if_absent(8, block_hash, b"second".to_vec())
            .await
            .unwrap();

        let reader = FileSystemReplayArchiveReader::new(archive_root.path().to_path_buf());
        let downloaded = download_all_replay_archive_objects(
            &reader,
            output_root.path(),
            DEFAULT_DOWNLOAD_CONCURRENCY,
        )
        .await
        .unwrap();
        assert_eq!(downloaded, 2);

        // A fully downloaded tree is a no-op to re-download.
        let downloaded = download_all_replay_archive_objects(
            &reader,
            output_root.path(),
            DEFAULT_DOWNLOAD_CONCURRENCY,
        )
        .await
        .unwrap();
        assert_eq!(downloaded, 0);

        // Simulate an interrupted download: one object exists only as a truncated
        // `.partial` leftover. It must be re-downloaded, not treated as complete.
        let record_path = output_root
            .path()
            .join("8")
            .join(crate::format_block_hash(block_hash));
        let partial_path = record_path.with_extension("partial");
        tokio::fs::remove_file(&record_path).await.unwrap();
        tokio::fs::write(&partial_path, b"sec").await.unwrap();

        let downloaded = download_all_replay_archive_objects(
            &reader,
            output_root.path(),
            DEFAULT_DOWNLOAD_CONCURRENCY,
        )
        .await
        .unwrap();
        assert_eq!(downloaded, 1);
        assert_eq!(tokio::fs::read(&record_path).await.unwrap(), b"second");
        assert!(!tokio::fs::try_exists(partial_path).await.unwrap());
    }

    #[tokio::test]
    async fn recover_records_to_rocksdb_walks_from_anchor_and_writes_node_format() {
        let input_root = tempfile::tempdir().unwrap();
        let replay_db = tempfile::tempdir().unwrap();
        let genesis_hash = B256::with_last_byte(1);
        let block_hash = B256::with_last_byte(2);
        let genesis_record = test_replay_record(0, B256::ZERO);
        let block_record = test_replay_record(1, genesis_hash);

        write_downloaded_replay_record(input_root.path(), 0, genesis_hash, &genesis_record).await;
        write_downloaded_replay_record(input_root.path(), 1, block_hash, &block_record).await;

        let recovered =
            recover_replay_records_to_rocksdb(input_root.path(), replay_db.path(), 1, block_hash)
                .await
                .unwrap();

        let replay_storage = BlockReplayStorage::new_without_genesis(replay_db.path(), 0);
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
            &genesis_record,
            &recipient,
        )
        .await;
        write_downloaded_encrypted_replay_record(
            input_root.path(),
            1,
            block_hash,
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
            Some(ArchiveIdentity::X25519(identity)),
            DEFAULT_DECRYPT_CONCURRENCY,
        )
        .await
        .unwrap();

        let replay_storage = BlockReplayStorage::new_without_genesis(replay_db.path(), 0);
        assert_eq!(recovered, 2);
        assert_eq!(replay_storage.get_replay_record(0).unwrap(), genesis_record);
        assert_eq!(replay_storage.get_replay_record(1).unwrap(), block_record);
    }

    #[tokio::test]
    async fn recover_records_ignores_non_block_hash_entries() {
        let input_root = tempfile::tempdir().unwrap();
        let replay_db = tempfile::tempdir().unwrap();
        let block_hash = B256::with_last_byte(1);
        let record = test_replay_record(0, B256::ZERO);

        write_downloaded_replay_record(input_root.path(), 0, block_hash, &record).await;
        // A stray directory next to the block hash files must not abort recovery.
        tokio::fs::create_dir(input_root.path().join("0").join("backup"))
            .await
            .unwrap();

        let recovered =
            recover_replay_records_to_rocksdb(input_root.path(), replay_db.path(), 0, block_hash)
                .await
                .unwrap();

        assert_eq!(recovered, 1);
    }

    #[tokio::test]
    async fn recover_records_skips_undecodable_non_canonical_records() {
        let input_root = tempfile::tempdir().unwrap();
        let replay_db = tempfile::tempdir().unwrap();
        let canonical_hash = B256::with_last_byte(1);
        let reorged_out_hash = B256::with_last_byte(2);
        let record = test_replay_record(0, B256::ZERO);

        write_downloaded_replay_record(input_root.path(), 0, canonical_hash, &record).await;
        // A record from a reorged-out block that cannot be decoded must not abort recovery
        // of the intact canonical chain.
        let reorged_out_path = input_root
            .path()
            .join("0")
            .join(format_block_hash(reorged_out_hash));
        tokio::fs::create_dir_all(reorged_out_path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(reorged_out_path, b"not a replay record")
            .await
            .unwrap();

        let recovered = recover_replay_records_to_rocksdb(
            input_root.path(),
            replay_db.path(),
            0,
            canonical_hash,
        )
        .await
        .unwrap();

        assert_eq!(recovered, 1);
    }

    #[tokio::test]
    async fn recover_records_ignores_partial_download_leftovers() {
        let input_root = tempfile::tempdir().unwrap();
        let replay_db = tempfile::tempdir().unwrap();
        let block_hash = B256::with_last_byte(1);
        let record = test_replay_record(0, B256::ZERO);

        write_downloaded_replay_record(input_root.path(), 0, block_hash, &record).await;
        // A truncated `.partial` file left by an interrupted download must not be decoded.
        let partial_path = input_root
            .path()
            .join("0")
            .join(format_block_hash(B256::with_last_byte(2)))
            .with_extension("partial");
        tokio::fs::write(partial_path, b"trunc").await.unwrap();

        let recovered =
            recover_replay_records_to_rocksdb(input_root.path(), replay_db.path(), 0, block_hash)
                .await
                .unwrap();

        assert_eq!(recovered, 1);
    }

    async fn write_downloaded_replay_record(
        input_root: &Path,
        block_number: BlockNumber,
        block_hash: BlockHash,
        record: &ReplayRecord,
    ) {
        let path = input_root
            .join(block_number.to_string())
            .join(format_block_hash(block_hash));
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
        record: &ReplayRecord,
        recipient: &age::x25519::Recipient,
    ) {
        let path = input_root
            .join(block_number.to_string())
            .join(format_block_hash(block_hash));
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
        let mut block_context = zksync_os_storage_api::BlockContext {
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
