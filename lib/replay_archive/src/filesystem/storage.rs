use crate::{ReplayArchiveStorage, ReplayArchiver, ReplayRecordArchiver, format_block_hash};
use alloy::primitives::{BlockHash, BlockNumber};
use anyhow::Context as _;
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Counter making concurrent temp file names unique within this process.
static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// File-system implementation of [`ReplayArchiveStorage`].
///
/// Objects are written to `<root>/<block_number>/<block_hash>`. A complete temporary file is
/// hard-linked to the final path so concurrent writers cannot overwrite each other and a crash
/// cannot leave a partial payload at the final key.
#[derive(Debug, Clone)]
pub struct FileSystemReplayArchiveStorage {
    root_path: PathBuf,
}

impl FileSystemReplayArchiveStorage {
    pub fn root_path(&self) -> &Path {
        &self.root_path
    }

    fn block_dir_path(&self, block_number: BlockNumber) -> PathBuf {
        self.root_path.join(block_number.to_string())
    }

    fn object_path(&self, block_number: BlockNumber, block_hash: BlockHash) -> PathBuf {
        self.block_dir_path(block_number)
            .join(format_block_hash(block_hash))
    }

    async fn put_object_file_if_absent(
        &self,
        object_path: &Path,
        object: &[u8],
    ) -> anyhow::Result<()> {
        let temp_path = temp_sibling_path(object_path);
        tokio::fs::write(&temp_path, object)
            .await
            .with_context(|| {
                format!(
                    "failed to write replay archive object temp file {}",
                    temp_path.display()
                )
            })?;
        let link_result = tokio::fs::hard_link(&temp_path, object_path).await;
        let cleanup_result = tokio::fs::remove_file(&temp_path).await;
        match link_result {
            Ok(()) => {
                cleanup_result.with_context(|| {
                    format!(
                        "failed to remove replay archive temp file {}",
                        temp_path.display()
                    )
                })?;
                Ok(())
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                cleanup_result.with_context(|| {
                    format!(
                        "failed to remove replay archive temp file {}",
                        temp_path.display()
                    )
                })?;
                Ok(())
            }
            Err(err) => {
                let _ = cleanup_result;
                Err(err).with_context(|| {
                    format!(
                        "failed to create replay archive object {}",
                        object_path.display()
                    )
                })
            }
        }
    }
}

/// Temp path next to `path` so the final hard link stays on one filesystem.
fn temp_sibling_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .expect("replay archive object path must have a file name")
        .to_string_lossy();
    path.with_file_name(format!(
        "{file_name}.partial-{}-{}",
        std::process::id(),
        TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
}

#[async_trait]
impl ReplayArchiveStorage for FileSystemReplayArchiveStorage {
    type Config = PathBuf;

    async fn init(root_path: Self::Config, _writer_node_id: String) -> anyhow::Result<Self> {
        tokio::fs::create_dir_all(&root_path)
            .await
            .with_context(|| {
                format!(
                    "failed to create replay archive root {}",
                    root_path.display()
                )
            })?;
        Ok(Self { root_path })
    }

    async fn put_object_if_absent(
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
        self.put_object_file_if_absent(&object_path, &object).await
    }

    async fn contains_object(
        &self,
        block_number: BlockNumber,
        block_hash: BlockHash,
    ) -> anyhow::Result<bool> {
        let object_path = self.object_path(block_number, block_hash);
        match tokio::fs::metadata(&object_path).await {
            Ok(metadata) => Ok(metadata.is_file()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
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

    pub async fn init(root_path: PathBuf, writer_node_id: String) -> anyhow::Result<Self> {
        let storage = FileSystemReplayArchiveStorage::init(root_path, writer_node_id).await?;
        Ok(Self::new(storage))
    }
}

#[async_trait]
impl ReplayArchiver for FileSystemReplayArchiver {
    async fn ensure_replay_record(
        &self,
        block_hash: BlockHash,
        replay_record: zksync_os_storage_api::ReplayRecord,
    ) -> anyhow::Result<()> {
        self.inner
            .ensure_replay_record(block_hash, replay_record)
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
