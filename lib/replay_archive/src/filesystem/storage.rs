use crate::{
    ReplayArchiveSession, ReplayArchiveStorage, ReplayArchiver, ReplayRecordArchiver,
    format_block_hash,
};
use alloy::primitives::{BlockHash, BlockNumber};
use anyhow::Context as _;
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt as _;

/// File-system implementation of [`ReplayArchiveStorage`].
///
/// The archive root is backend configuration. The session is created as a direct child of that root,
/// and objects are written to `<root>/<session>/<block_number>/<block_hash>`.
#[derive(Debug, Clone)]
pub struct FileSystemReplayArchiveStorage {
    root_path: PathBuf,
    session: ReplayArchiveSession,
}

impl FileSystemReplayArchiveStorage {
    pub fn root_path(&self) -> &Path {
        &self.root_path
    }

    pub fn session(&self) -> &ReplayArchiveSession {
        &self.session
    }

    fn session_path(&self) -> PathBuf {
        self.root_path.join(self.session.folder_name())
    }

    fn block_dir_path(&self, block_number: BlockNumber) -> PathBuf {
        self.session_path().join(block_number.to_string())
    }

    fn object_path(&self, block_number: BlockNumber, block_hash: BlockHash) -> PathBuf {
        self.block_dir_path(block_number)
            .join(format_block_hash(block_hash))
    }
}

#[async_trait]
impl ReplayArchiveStorage for FileSystemReplayArchiveStorage {
    type Config = PathBuf;

    async fn init(root_path: Self::Config, session: ReplayArchiveSession) -> anyhow::Result<Self> {
        tokio::fs::create_dir_all(&root_path)
            .await
            .with_context(|| {
                format!(
                    "failed to create replay archive root {}",
                    root_path.display()
                )
            })?;

        let session_path = root_path.join(session.folder_name());
        tokio::fs::create_dir(&session_path)
            .await
            .with_context(|| {
                format!(
                    "failed to create append-only replay archive session {}",
                    session_path.display()
                )
            })?;

        Ok(Self { root_path, session })
    }

    async fn append_object(
        &self,
        block_number: BlockNumber,
        block_hash: BlockHash,
        object: Vec<u8>,
    ) -> anyhow::Result<()> {
        let block_dir_path = self.block_dir_path(block_number);
        tokio::fs::create_dir_all(&block_dir_path)
            .await
            .with_context(|| {
                format!(
                    "failed to create replay archive block directory {}",
                    block_dir_path.display()
                )
            })?;

        let object_path = self.object_path(block_number, block_hash);
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&object_path)
            .await
            .with_context(|| {
                format!(
                    "failed to create append-only replay archive object {}",
                    object_path.display()
                )
            })?;
        file.write_all(&object).await.with_context(|| {
            format!(
                "failed to write replay archive object {}",
                object_path.display()
            )
        })?;
        file.flush().await.with_context(|| {
            format!(
                "failed to flush replay archive object {}",
                object_path.display()
            )
        })?;
        Ok(())
    }

    async fn contains_object(
        &self,
        block_number: BlockNumber,
        block_hash: BlockHash,
    ) -> anyhow::Result<bool> {
        let object_path = self.object_path(block_number, block_hash);
        match tokio::fs::metadata(&object_path).await {
            Ok(metadata) => Ok(metadata.is_file()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(err) => Err(err).with_context(|| {
                format!(
                    "failed to read replay archive object metadata {}",
                    object_path.display()
                )
            }),
        }
    }
}

/// File-system replay archiver that stores plaintext JSON replay records.
#[derive(Debug, Clone)]
pub struct FileSystemReplayArchiver {
    inner: ReplayRecordArchiver<FileSystemReplayArchiveStorage>,
}

impl FileSystemReplayArchiver {
    pub fn new(storage: FileSystemReplayArchiveStorage) -> Self {
        Self {
            inner: ReplayRecordArchiver::new(storage),
        }
    }

    pub async fn init(root_path: PathBuf, session: ReplayArchiveSession) -> anyhow::Result<Self> {
        let storage = FileSystemReplayArchiveStorage::init(root_path, session).await?;
        Ok(Self::new(storage))
    }
}

#[async_trait]
impl ReplayArchiver for FileSystemReplayArchiver {
    async fn append_replay_record(
        &self,
        block_hash: BlockHash,
        replay_record: zksync_os_storage_api::ReplayRecord,
    ) -> anyhow::Result<()> {
        self.inner
            .append_replay_record(block_hash, replay_record)
            .await
    }

    async fn contains_replay_record(
        &self,
        block_number: BlockNumber,
        block_hash: BlockHash,
    ) -> anyhow::Result<bool> {
        self.inner
            .contains_replay_record(block_number, block_hash)
            .await
    }
}
