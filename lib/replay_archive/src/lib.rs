use alloy::primitives::{BlockHash, BlockNumber};
use async_trait::async_trait;
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;
use zksync_os_storage_api::ReplayRecord;

mod age_encrypted;
mod component;
mod filesystem;
mod gate_component;
mod init;
mod metrics;
mod reader;
mod recovery;
mod replay_record;
mod write_replay;

pub use age_encrypted::AgeEncryptedReplayArchiver;
pub use component::{ReplayArchiveComponent, ReplayArchiveRecord, ReplayArchiveSender};
pub use filesystem::{
    FileSystemReplayArchiveReader, FileSystemReplayArchiveStorage, FileSystemReplayArchiver,
};
pub use gate_component::ReplayArchiveGateComponent;
pub use init::{
    InitializedReplayArchive, ReplayArchiveConfig, ReplayArchiveEncryptionConfig,
    init_replay_archive,
};
pub use reader::{ReplayArchiveObject, ReplayArchiveObjectStream, ReplayArchiveStorageReader};
pub use recovery::{
    download_all_replay_archive_objects, parse_age_x25519_identity, read_age_x25519_identity,
    recover_replay_records_to_rocksdb, recover_replay_records_to_rocksdb_with_optional_decryption,
};
pub use replay_record::ReplayRecordArchiver;
pub use write_replay::ReplayArchivingWriteReplay;

pub const REPLAY_ARCHIVE_QUEUE_SIZE: usize = 128;

/// Replay archive layout:
///
/// ```text
/// <timestamp_millis>-<node_id>/<block_number>/<block_hash>
/// ```
///
/// The stored object value is the replay record bytes chosen by a concrete backend. No batch
/// metadata or extra envelope is part of the value; all lookup metadata is encoded in the key.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ReplayArchiveSession {
    timestamp_millis: u64,
    node_id: String,
}

impl ReplayArchiveSession {
    pub fn new(
        timestamp_millis: u64,
        node_id: impl Into<String>,
    ) -> Result<Self, InvalidReplayArchiveSession> {
        let node_id = node_id.into();
        validate_node_id(&node_id)?;
        Ok(Self {
            timestamp_millis,
            node_id,
        })
    }

    pub fn node_id(&self) -> &str {
        &self.node_id
    }

    pub fn timestamp_millis(&self) -> u64 {
        self.timestamp_millis
    }

    pub fn folder_name(&self) -> String {
        format!("{}-{}", self.timestamp_millis, self.node_id)
    }
}

impl fmt::Display for ReplayArchiveSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.folder_name())
    }
}

impl FromStr for ReplayArchiveSession {
    type Err = InvalidReplayArchiveSession;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (timestamp_millis, node_id) = value
            .split_once('-')
            .ok_or(InvalidReplayArchiveSession::MissingTimestamp)?;
        let timestamp_millis = timestamp_millis
            .parse()
            .map_err(|_| InvalidReplayArchiveSession::InvalidTimestamp)?;
        Self::new(timestamp_millis, node_id)
    }
}

/// Full storage key for a single replay record object.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ReplayArchiveKey {
    pub session: ReplayArchiveSession,
    pub block_number: BlockNumber,
    pub block_hash: BlockHash,
}

impl ReplayArchiveKey {
    pub fn new(
        session: ReplayArchiveSession,
        block_number: BlockNumber,
        block_hash: BlockHash,
    ) -> Self {
        Self {
            session,
            block_number,
            block_hash,
        }
    }

    pub fn object_path(&self) -> String {
        format!(
            "{}/{}/{}",
            self.session,
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

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum InvalidReplayArchiveSession {
    #[error("replay archive node id cannot be empty")]
    EmptyNodeId,
    #[error("replay archive node id cannot contain path separators")]
    NodeIdContainsPathSeparator,
    #[error("replay archive session name must start with <timestamp_millis>-")]
    MissingTimestamp,
    #[error("replay archive session timestamp must be an unsigned integer")]
    InvalidTimestamp,
}

fn validate_node_id(node_id: &str) -> Result<(), InvalidReplayArchiveSession> {
    if node_id.is_empty() {
        return Err(InvalidReplayArchiveSession::EmptyNodeId);
    }
    if node_id.contains('/') || node_id.contains('\\') {
        return Err(InvalidReplayArchiveSession::NodeIdContainsPathSeparator);
    }
    Ok(())
}

fn format_block_hash(block_hash: BlockHash) -> String {
    alloy::hex::encode_prefixed(block_hash.0)
}

/// Session-bound byte storage using the session/block/hash layout.
///
/// Implementations must be append-only. Creating storage must create or mark exactly one session
/// and must fail if that session already exists. Appending an object must fail if data already
/// exists at `<session>/<block_number>/<block_hash>`, even if the stored value is byte-for-byte
/// identical. This is intentionally stricter than idempotent object-store writes so that bugs do
/// not silently replace archived replay data.
#[async_trait]
pub trait ReplayArchiveStorage: Sized + Send + Sync + 'static {
    /// Backend-specific configuration needed to create session-bound storage.
    type Config: Send;

    /// Initializes or marks `<session>/` and returns storage bound to that session.
    ///
    /// Object stores that do not have real directories should write a marker object or use another
    /// backend-specific existence check. Returning success for an already existing session violates
    /// the append-only contract.
    async fn init(config: Self::Config, session: ReplayArchiveSession) -> anyhow::Result<Self>;

    /// Appends `object` at `<session>/<block_number>/<block_hash>`.
    ///
    /// Implementations must not overwrite any existing object at this key.
    async fn append_object(
        &self,
        block_number: BlockNumber,
        block_hash: BlockHash,
        object: Vec<u8>,
    ) -> anyhow::Result<()>;

    /// Checks whether an object exists at `<session>/<block_number>/<block_hash>`.
    async fn contains_object(
        &self,
        block_number: BlockNumber,
        block_hash: BlockHash,
    ) -> anyhow::Result<bool>;
}

/// Session-bound archive for replay records.
#[async_trait]
pub trait ReplayArchiver: Send + Sync + 'static {
    /// Appends `replay_record` at
    /// `<session>/<replay_record.block_context.block_number>/<block_hash>`.
    async fn append_replay_record(
        &self,
        block_hash: BlockHash,
        replay_record: ReplayRecord,
    ) -> anyhow::Result<()>;

    /// Checks whether an archived object exists at `<session>/<block_number>/<block_hash>`.
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
    async fn append_replay_record(
        &self,
        block_hash: BlockHash,
        replay_record: ReplayRecord,
    ) -> anyhow::Result<()> {
        self.as_ref()
            .append_replay_record(block_hash, replay_record)
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
    fn session_roundtrips_with_hyphenated_node_id() {
        let session = ReplayArchiveSession::new(42, "node-a").unwrap();

        assert_eq!(session.folder_name(), "42-node-a");
        assert_eq!(
            "42-node-a".parse::<ReplayArchiveSession>().unwrap(),
            session
        );
    }

    #[test]
    fn key_uses_expected_layout() {
        let session = ReplayArchiveSession::new(42, "node-a").unwrap();
        let key = ReplayArchiveKey::new(session, 7, B256::ZERO);

        assert_eq!(
            key.object_path(),
            "42-node-a/7/0x0000000000000000000000000000000000000000000000000000000000000000"
        );
    }

    #[tokio::test]
    async fn filesystem_archive_creates_session_once() {
        let tempdir = tempfile::tempdir().unwrap();
        let session = ReplayArchiveSession::new(42, "node-a").unwrap();

        FileSystemReplayArchiveStorage::init(tempdir.path().to_path_buf(), session.clone())
            .await
            .unwrap();

        let err = FileSystemReplayArchiveStorage::init(tempdir.path().to_path_buf(), session)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("failed to create append-only"));
    }

    #[tokio::test]
    async fn filesystem_archive_appends_and_checks_presence() {
        let tempdir = tempfile::tempdir().unwrap();
        let session = ReplayArchiveSession::new(42, "node-a").unwrap();
        let storage = FileSystemReplayArchiveStorage::init(tempdir.path().to_path_buf(), session)
            .await
            .unwrap();
        let archive = FileSystemReplayArchiver::new(storage);
        let block_hash = B256::with_last_byte(1);
        let replay_record = test_replay_record(7);

        assert!(!archive.contains_replay_record(7, block_hash).await.unwrap());

        archive
            .append_replay_record(block_hash, replay_record.clone())
            .await
            .unwrap();

        assert!(archive.contains_replay_record(7, block_hash).await.unwrap());

        archive
            .append_replay_record(block_hash, replay_record)
            .await
            .unwrap_err();
    }

    #[test]
    fn age_encrypted_archive_encrypts_replay_record_for_recipient() {
        let identity = age::x25519::Identity::generate();
        let recipient = identity.to_public();
        let archive = AgeEncryptedReplayArchiver::new((), recipient);
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
