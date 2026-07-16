use crate::{
    ArchiveObjectMeta, ArchiveOutcome, PutOutcome, ReplayArchiveStorage, ReplayArchiver,
    ReplayRecordArchiver, format_block_hash,
};
use alloy::primitives::{BlockHash, BlockNumber};
use anyhow::Context as _;
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Extension of the sidecar file holding an object's identity digest.
pub(crate) const DIGEST_SIDECAR_EXTENSION: &str = "digest";

/// Counter making concurrent temp file names unique within this process.
static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// File-system implementation of [`ReplayArchiveStorage`].
///
/// Objects are written to `<root>/<block_number>/<block_hash>` with the identity digest in a
/// `<block_hash>.digest` sidecar. Object stores attach the digest atomically as object
/// metadata; a filesystem cannot create two files atomically, so the sidecar acts as the
/// atomic claim instead: it is hard-linked into place fully written, before the data file.
/// A crash between the two leaves a sidecar without data, which a later put with the same
/// digest self-heals by rewriting the data file.
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

    fn digest_path(&self, block_number: BlockNumber, block_hash: BlockHash) -> PathBuf {
        self.block_dir_path(block_number).join(format!(
            "{}.{DIGEST_SIDECAR_EXTENSION}",
            format_block_hash(block_hash)
        ))
    }

    /// Atomically claims the digest sidecar for this writer.
    ///
    /// Returns `true` if this call created the sidecar and `false` if a sidecar already
    /// existed. The sidecar is hard-linked into place only after its content is fully
    /// written, so a concurrent reader never observes a partially written digest.
    async fn claim_digest_sidecar(
        &self,
        digest_path: &Path,
        identity_digest: &str,
    ) -> anyhow::Result<bool> {
        let temp_path = temp_sibling_path(digest_path);
        tokio::fs::write(&temp_path, identity_digest)
            .await
            .with_context(|| {
                format!(
                    "failed to write replay archive digest temp file {}",
                    temp_path.display()
                )
            })?;
        let link_result = tokio::fs::hard_link(&temp_path, digest_path).await;
        let cleanup = tokio::fs::remove_file(&temp_path).await;
        match link_result {
            Ok(()) => {
                cleanup.with_context(|| {
                    format!(
                        "failed to remove replay archive digest temp file {}",
                        temp_path.display()
                    )
                })?;
                Ok(true)
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
            Err(err) => Err(err).with_context(|| {
                format!(
                    "failed to claim replay archive digest sidecar {}",
                    digest_path.display()
                )
            }),
        }
    }

    async fn write_object_file(&self, object_path: &Path, object: &[u8]) -> anyhow::Result<()> {
        let temp_path = temp_sibling_path(object_path);
        tokio::fs::write(&temp_path, object)
            .await
            .with_context(|| {
                format!(
                    "failed to write replay archive object temp file {}",
                    temp_path.display()
                )
            })?;
        tokio::fs::rename(&temp_path, object_path)
            .await
            .with_context(|| {
                format!(
                    "failed to finalize replay archive object {}",
                    object_path.display()
                )
            })
    }
}

/// Temp path next to `path` so the final `rename`/`hard_link` stays on one filesystem.
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

    async fn put_new_object(
        &self,
        block_number: BlockNumber,
        block_hash: BlockHash,
        object: Vec<u8>,
        identity_digest: &str,
    ) -> anyhow::Result<PutOutcome> {
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
        let digest_path = self.digest_path(block_number, block_hash);

        if self
            .claim_digest_sidecar(&digest_path, identity_digest)
            .await?
        {
            self.write_object_file(&object_path, &object).await?;
            return Ok(PutOutcome::Created);
        }

        let stored_digest = tokio::fs::read_to_string(&digest_path)
            .await
            .with_context(|| {
                format!(
                    "failed to read replay archive digest sidecar {}",
                    digest_path.display()
                )
            })?;
        anyhow::ensure!(
            stored_digest == identity_digest,
            "replay archive divergence detected for block #{block_number}, {block_hash}: \
             local record identity digest {identity_digest} does not match archived digest \
             {stored_digest}"
        );
        if !tokio::fs::try_exists(&object_path).await? {
            // A previous writer crashed after claiming the sidecar but before finalizing the
            // data file. The digests match, so rewriting the payload restores the object.
            self.write_object_file(&object_path, &object).await?;
            return Ok(PutOutcome::Created);
        }
        Ok(PutOutcome::AlreadyExists)
    }

    async fn stored_object_meta(
        &self,
        block_number: BlockNumber,
        block_hash: BlockHash,
    ) -> anyhow::Result<Option<ArchiveObjectMeta>> {
        let object_path = self.object_path(block_number, block_hash);
        match tokio::fs::metadata(&object_path).await {
            Ok(metadata) if metadata.is_file() => {}
            Ok(_) => return Ok(None),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(err).with_context(|| {
                    format!(
                        "failed to read replay archive object metadata {}",
                        object_path.display()
                    )
                });
            }
        }

        let digest_path = self.digest_path(block_number, block_hash);
        let identity_digest = match tokio::fs::read_to_string(&digest_path).await {
            Ok(digest) => Some(digest),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => {
                return Err(err).with_context(|| {
                    format!(
                        "failed to read replay archive digest sidecar {}",
                        digest_path.display()
                    )
                });
            }
        };
        Ok(Some(ArchiveObjectMeta { identity_digest }))
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
    ) -> anyhow::Result<ArchiveOutcome> {
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
