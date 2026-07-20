use crate::{
    ReplayArchiveKey, ReplayArchiveKeyPage, ReplayArchiveStorageReader, format_block_hash,
};
use alloy::primitives::{BlockHash, BlockNumber};
use anyhow::Context as _;
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::str::FromStr as _;

/// File-system implementation of [`ReplayArchiveStorageReader`].
///
/// Lists the flat layout (`<root>/<block_number>/<block_hash>`).
#[derive(Debug, Clone)]
pub struct FileSystemReplayArchiveReader {
    root_path: PathBuf,
}

impl FileSystemReplayArchiveReader {
    pub fn new(root_path: PathBuf) -> Self {
        Self { root_path }
    }

    pub fn root_path(&self) -> &Path {
        &self.root_path
    }

    fn object_path(&self, key: &ReplayArchiveKey) -> PathBuf {
        self.root_path
            .join(key.block_number.to_string())
            .join(format_block_hash(key.block_hash))
    }

    async fn list_block_dir_objects(
        &self,
        block_dir: &Path,
        block_number: BlockNumber,
        keys: &mut Vec<ReplayArchiveKey>,
    ) -> anyhow::Result<()> {
        let mut object_entries = tokio::fs::read_dir(block_dir).await.with_context(|| {
            format!(
                "failed to read replay archive block directory {}",
                block_dir.display()
            )
        })?;
        while let Some(object_entry) = object_entries.next_entry().await.with_context(|| {
            format!(
                "failed to read replay archive object entry {}",
                block_dir.display()
            )
        })? {
            if !object_entry.file_type().await?.is_file() {
                continue;
            }
            let file_name = object_entry.file_name();
            let Some(file_name) = file_name.to_str() else {
                continue;
            };
            // Interrupted-write leftovers accompany data files in the flat layout; only plain
            // block hash names are archive objects.
            let Ok(block_hash) = BlockHash::from_str(file_name) else {
                if !file_name.contains(".partial") {
                    tracing::warn!(
                        path = %object_entry.path().display(),
                        "Skipping replay archive entry that is not a block hash"
                    );
                }
                continue;
            };
            keys.push(ReplayArchiveKey::new(block_number, block_hash));
        }
        Ok(())
    }
}

#[async_trait]
impl ReplayArchiveStorageReader for FileSystemReplayArchiveReader {
    // The local filesystem backend does not paginate: the first page contains every key.
    async fn list_keys_page(
        &self,
        page_token: Option<String>,
    ) -> anyhow::Result<ReplayArchiveKeyPage> {
        anyhow::ensure!(
            page_token.is_none(),
            "filesystem replay archive reader returns a single page"
        );
        let mut keys = Vec::new();
        let mut root_entries = tokio::fs::read_dir(&self.root_path)
            .await
            .with_context(|| {
                format!(
                    "failed to read replay archive root {}",
                    self.root_path.display()
                )
            })?;

        while let Some(root_entry) = root_entries.next_entry().await.with_context(|| {
            format!(
                "failed to read replay archive root entry {}",
                self.root_path.display()
            )
        })? {
            if !root_entry.file_type().await?.is_dir() {
                continue;
            }
            let dir_name = root_entry.file_name();
            let Some(dir_name) = dir_name.to_str() else {
                continue;
            };

            let Ok(block_number) = dir_name.parse::<BlockNumber>() else {
                tracing::warn!(
                    path = %root_entry.path().display(),
                    "Skipping replay archive root entry that is not a block number"
                );
                continue;
            };
            self.list_block_dir_objects(&root_entry.path(), block_number, &mut keys)
                .await?;
        }

        Ok(ReplayArchiveKeyPage {
            keys,
            next_page_token: None,
        })
    }

    async fn fetch_object(&self, key: &ReplayArchiveKey) -> anyhow::Result<Vec<u8>> {
        let path = self.object_path(key);
        tokio::fs::read(&path)
            .await
            .with_context(|| format!("failed to read replay archive object {}", path.display()))
    }
}
