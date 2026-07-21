use alloy::primitives::{BlockHash, BlockNumber};
use async_trait::async_trait;
use std::fmt;
use std::sync::Arc;
use zksync_os_storage_api::ReplayRecord;

mod age_encrypted;
mod component;
mod filesystem;
mod gate_component;
#[cfg(feature = "gcp")]
mod gcs;
mod init;
#[cfg(feature = "gcp")]
mod kms;
mod metrics;
mod reader;
mod recovery;
mod replay_record;
mod s3;
mod write_replay;

pub use age_encrypted::{AgeEncryptedReplayArchiver, ArchiveRecipient};
pub use component::{ReplayArchiveComponent, ReplayArchiveRecord, ReplayArchiveSender};
pub use filesystem::{
    FileSystemReplayArchiveReader, FileSystemReplayArchiveStorage, FileSystemReplayArchiver,
};
pub use gate_component::ReplayArchiveGateComponent;
#[cfg(feature = "gcp")]
pub use gcs::{
    GcsReplayArchiveAuthMode, GcsReplayArchiveConfig, GcsReplayArchiveReader,
    GcsReplayArchiveStorage,
};
pub use init::{
    InitializedReplayArchive, ReplayArchiveConfig, ReplayArchiveEncryptionConfig,
    init_replay_archive,
};
#[cfg(feature = "gcp")]
pub use kms::{GcpKmsClient, GcpKmsConfig, GcpKmsIdentity, GcpKmsRecipient};
pub use reader::{ReplayArchiveKeyPage, ReplayArchiveStorageReader};
pub use recovery::{
    ArchiveIdentity, DEFAULT_DECRYPT_CONCURRENCY, DEFAULT_DOWNLOAD_CONCURRENCY,
    download_all_replay_archive_objects, parse_age_x25519_identity, read_age_x25519_identity,
    recover_replay_records_to_rocksdb, recover_replay_records_to_rocksdb_with_optional_decryption,
};
pub use replay_record::ReplayRecordArchiver;
pub use s3::{
    S3ReplayArchiveAuthMode, S3ReplayArchiveConfig, S3ReplayArchiveReader, S3ReplayArchiveStorage,
};
pub use write_replay::ReplayArchivingWriteReplay;

pub const REPLAY_ARCHIVE_QUEUE_SIZE: usize = 128;

/// Replay archive layout:
///
/// ```text
/// <block_number>/<block_hash>
/// ```
///
/// Every node writes into the same flat namespace; conditional create-if-absent (GCS
/// `if_generation_match(0)`, S3 `If-None-Match: *`) guarantees exactly one stored copy per block.
/// Full storage key for a single replay record object.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ReplayArchiveKey {
    pub block_number: BlockNumber,
    pub block_hash: BlockHash,
}

impl ReplayArchiveKey {
    pub fn new(block_number: BlockNumber, block_hash: BlockHash) -> Self {
        Self {
            block_number,
            block_hash,
        }
    }

    pub fn object_path(&self) -> String {
        format!(
            "{}/{}",
            self.block_number,
            format_block_hash(self.block_hash)
        )
    }
}

impl fmt::Display for ReplayArchiveKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.object_path())
    }
}

fn format_block_hash(block_hash: BlockHash) -> String {
    alloy::hex::encode_prefixed(block_hash.0)
}

/// Parses a flat object-store key back into a [`ReplayArchiveKey`].
///
/// Keys that do not match `<block_number>/<block_hash>` are logged and skipped instead of
/// failing the listing because a shared bucket may hold foreign objects.
pub(crate) fn parse_archive_object_key(object_key: &str) -> Option<ReplayArchiveKey> {
    let parts = object_key.split('/').collect::<Vec<_>>();
    let key = match parts.as_slice() {
        [block_number, block_hash] => match (
            block_number.parse::<BlockNumber>(),
            block_hash.parse::<BlockHash>(),
        ) {
            (Ok(block_number), Ok(block_hash)) => {
                Some(ReplayArchiveKey::new(block_number, block_hash))
            }
            _ => None,
        },
        _ => None,
    };
    if key.is_none() {
        tracing::warn!(
            object_key,
            "Skipping object key that is not a replay archive record"
        );
    }
    key
}

/// Shared byte storage using the flat `<block_number>/<block_hash>` layout.
///
/// Multiple nodes write into the same namespace concurrently. Implementations must guarantee
/// that [`ReplayArchiveStorage::put_object_if_absent`] creates the object only if no object exists
/// at the key yet. An existing object is a successful no-op and must never be overwritten.
#[async_trait]
pub trait ReplayArchiveStorage: Sized + Send + Sync + 'static {
    /// Backend-specific configuration needed to create the storage.
    type Config: Send;

    /// Initializes storage access.
    ///
    /// `writer_node_id` identifies this node in forensic object metadata; it plays no role in
    /// the object layout.
    async fn init(config: Self::Config, writer_node_id: String) -> anyhow::Result<Self>;

    /// Creates `object` at `<block_number>/<block_hash>` if the key is still free.
    ///
    /// Returns success without overwriting when another writer already created the key.
    async fn put_object_if_absent(
        &self,
        block_number: BlockNumber,
        block_hash: BlockHash,
        object: Vec<u8>,
    ) -> anyhow::Result<()>;

    /// Checks whether an object exists at `<block_number>/<block_hash>`.
    async fn contains_object(
        &self,
        block_number: BlockNumber,
        block_hash: BlockHash,
    ) -> anyhow::Result<bool>;
}

/// Ensures an object is archived at `<block_number>/<block_hash>`, encoding the payload lazily
/// only if the key is not already present.
pub(crate) async fn ensure_object_archived<Storage, Encode>(
    storage: &Storage,
    block_number: BlockNumber,
    block_hash: BlockHash,
    encode: Encode,
) -> anyhow::Result<()>
where
    Storage: ReplayArchiveStorage,
    Encode: FnOnce() -> anyhow::Result<Vec<u8>> + Send,
{
    if storage.contains_object(block_number, block_hash).await? {
        return Ok(());
    }

    let object = encode()?;
    storage
        .put_object_if_absent(block_number, block_hash, object)
        .await
}

/// Archive for replay records over the shared flat layout.
#[async_trait]
pub trait ReplayArchiver: Send + Sync + 'static {
    /// Ensures `replay_record` is archived at
    /// `<replay_record.block_context.block_number>/<block_hash>`.
    async fn ensure_replay_record(
        &self,
        block_hash: BlockHash,
        replay_record: ReplayRecord,
    ) -> anyhow::Result<()>;

    /// Checks whether an archived object exists at `<block_number>/<block_hash>`.
    ///
    /// This intentionally verifies presence only. Encrypted archive objects are randomized, so
    /// implementations must not depend on re-encrypting and comparing bytes.
    async fn contains_replay_record(
        &self,
        block_number: BlockNumber,
        block_hash: BlockHash,
    ) -> anyhow::Result<bool>;
}

#[async_trait]
impl<T> ReplayArchiver for Arc<T>
where
    T: ReplayArchiver + ?Sized,
{
    async fn ensure_replay_record(
        &self,
        block_hash: BlockHash,
        replay_record: ReplayRecord,
    ) -> anyhow::Result<()> {
        self.as_ref()
            .ensure_replay_record(block_hash, replay_record)
            .await
    }

    async fn contains_replay_record(
        &self,
        block_number: BlockNumber,
        block_hash: BlockHash,
    ) -> anyhow::Result<bool> {
        self.as_ref()
            .contains_replay_record(block_number, block_hash)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replay_record::encode_replay_record;
    use alloy::primitives::B256;
    use zksync_os_storage_api::ReplayRecord;

    #[test]
    fn archive_key_roundtrips_flat_layout() {
        let key = ReplayArchiveKey::new(7, B256::ZERO);

        assert_eq!(
            key.object_path(),
            "7/0x0000000000000000000000000000000000000000000000000000000000000000"
        );
        assert_eq!(parse_archive_object_key(&key.object_path()), Some(key));
    }

    #[test]
    fn archive_object_key_parser_skips_foreign_keys() {
        // A shared bucket may hold unrelated objects; they must be skipped, not abort listing.
        assert!(parse_archive_object_key("logs/2026/app.txt").is_none());
        assert!(
            parse_archive_object_key(
                "prefix/7/0x0000000000000000000000000000000000000000000000000000000000000001"
            )
            .is_none()
        );
        assert!(parse_archive_object_key("42-node-a/7/not-a-hash").is_none());
        assert!(parse_archive_object_key("42-node-a/not-a-number/0x00").is_none());
        assert!(parse_archive_object_key("7/not-a-hash").is_none());
        assert!(parse_archive_object_key("single-segment").is_none());
    }

    #[tokio::test]
    async fn concurrent_filesystem_writers_keep_one_complete_object() {
        let tempdir = tempfile::tempdir().unwrap();
        let storage =
            FileSystemReplayArchiveStorage::init(tempdir.path().to_path_buf(), "node-a".to_owned())
                .await
                .unwrap();
        let block_hash = B256::with_last_byte(1);

        let first = storage.put_object_if_absent(7, block_hash, b"first payload".to_vec());
        let second = storage.put_object_if_absent(7, block_hash, b"second payload".to_vec());
        let (first, second) = tokio::join!(first, second);
        first.unwrap();
        second.unwrap();

        let reader = FileSystemReplayArchiveReader::new(tempdir.path().to_path_buf());
        let stored = reader
            .fetch_object(&ReplayArchiveKey::new(7, block_hash))
            .await
            .unwrap();
        assert!(stored == b"first payload" || stored == b"second payload");
    }

    #[tokio::test]
    async fn existing_object_skips_payload_encoding() {
        let tempdir = tempfile::tempdir().unwrap();
        let storage =
            FileSystemReplayArchiveStorage::init(tempdir.path().to_path_buf(), "node-a".to_owned())
                .await
                .unwrap();
        let block_hash = B256::with_last_byte(1);
        storage
            .put_object_if_absent(7, block_hash, b"stored".to_vec())
            .await
            .unwrap();

        ensure_object_archived(&storage, 7, block_hash, || {
            panic!("payload should not be encoded when the archive object exists")
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn filesystem_archive_keeps_first_record_for_same_block_key() {
        let tempdir = tempfile::tempdir().unwrap();
        let storage =
            FileSystemReplayArchiveStorage::init(tempdir.path().to_path_buf(), "node-a".to_owned())
                .await
                .unwrap();
        let archive = FileSystemReplayArchiver::new(storage);
        let block_hash = B256::with_last_byte(1);
        let replay_record = test_replay_record(7);

        archive
            .ensure_replay_record(block_hash, replay_record.clone())
            .await
            .unwrap();

        let mut other_record = replay_record.clone();
        other_record.block_output_hash = B256::with_last_byte(2);
        archive
            .ensure_replay_record(block_hash, other_record)
            .await
            .unwrap();

        let reader = FileSystemReplayArchiveReader::new(tempdir.path().to_path_buf());
        let stored = reader
            .fetch_object(&ReplayArchiveKey::new(7, block_hash))
            .await
            .unwrap();
        let stored: ReplayRecord = serde_json::from_slice(&stored).unwrap();
        assert_eq!(stored, replay_record);
    }

    #[test]
    fn age_encrypted_archive_encrypts_replay_record_for_recipient() {
        let identity = age::x25519::Identity::generate();
        let recipient = identity.to_public();
        let archive = AgeEncryptedReplayArchiver::new((), ArchiveRecipient::X25519(recipient));
        let replay_record = test_replay_record(7);

        let encrypted = archive.encrypt_replay_record(&replay_record).unwrap();
        let encoded = encode_replay_record(&replay_record);

        assert_ne!(encrypted, encoded);
        let decrypted = age::decrypt(&identity, encrypted.as_slice()).unwrap();
        assert_eq!(decrypted, encoded);
    }

    fn test_replay_record(block_number: u64) -> ReplayRecord {
        ReplayRecord {
            block_context: zksync_os_storage_api::BlockContext {
                block_number,
                ..Default::default()
            },
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
