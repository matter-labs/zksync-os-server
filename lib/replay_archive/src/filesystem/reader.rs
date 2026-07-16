use super::storage::DIGEST_SIDECAR_EXTENSION;
use crate::{
    ReplayArchiveKey, ReplayArchiveKeyPage, ReplayArchiveSession, ReplayArchiveStorageReader,
    format_block_hash,
};
use alloy::primitives::{BlockHash, BlockNumber};
use anyhow::Context as _;
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::str::FromStr as _;

/// File-system implementation of [`ReplayArchiveStorageReader`].
///
/// Lists both the current flat layout (`<root>/<block_number>/<block_hash>`) and the legacy
/// session layout (`<root>/<session>/<block_number>/<block_hash>`): archive roots written
/// before the flat layout hold session directories, and both can coexist during migration.
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
        let mut path = self.root_path.clone();
        if let Some(session) = &key.session {
            path = path.join(session.folder_name());
        }
        path.join(key.block_number.to_string())
            .join(format_block_hash(key.block_hash))
    }

    async fn list_block_dir_objects(
        &self,
        block_dir: &Path,
        session: Option<&ReplayArchiveSession>,
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
            // Digest sidecars and interrupted-write leftovers accompany data files in the
            // flat layout; only plain block hash names are archive objects.
            let Ok(block_hash) = BlockHash::from_str(file_name) else {
                if !file_name.ends_with(&format!(".{DIGEST_SIDECAR_EXTENSION}"))
                    && !file_name.contains(".partial")
                {
                    tracing::warn!(
                        path = %object_entry.path().display(),
                        "Skipping replay archive entry that is not a block hash"
                    );
                }
                continue;
            };
            keys.push(ReplayArchiveKey {
                session: session.cloned(),
                block_number,
                block_hash,
            });
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

            if let Ok(block_number) = dir_name.parse::<BlockNumber>() {
                self.list_block_dir_objects(&root_entry.path(), None, block_number, &mut keys)
                    .await?;
                continue;
            }

            let Ok(session) = dir_name.parse::<ReplayArchiveSession>() else {
                tracing::warn!(
                    path = %root_entry.path().display(),
                    "Skipping replay archive root entry that is neither a block number nor a session"
                );
                continue;
            };

            let mut block_entries =
                tokio::fs::read_dir(root_entry.path())
                    .await
                    .with_context(|| {
                        format!(
                            "failed to read replay archive session {}",
                            root_entry.path().display()
                        )
                    })?;
            while let Some(block_entry) = block_entries.next_entry().await.with_context(|| {
                format!(
                    "failed to read replay archive session entry {}",
                    root_entry.path().display()
                )
            })? {
                if !block_entry.file_type().await?.is_dir() {
                    continue;
                }
                let block_number = parse_block_number_entry(&block_entry)?;
                self.list_block_dir_objects(
                    &block_entry.path(),
                    Some(&session),
                    block_number,
                    &mut keys,
                )
                .await?;
            }
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

fn parse_block_number_entry(entry: &tokio::fs::DirEntry) -> anyhow::Result<BlockNumber> {
    entry
        .file_name()
        .to_str()
        .context("replay archive block number path is not valid UTF-8")?
        .parse()
        .with_context(|| {
            format!(
                "failed to parse replay archive block number {}",
                entry.path().display()
            )
        })
}
