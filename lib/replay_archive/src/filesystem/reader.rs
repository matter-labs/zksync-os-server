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
            .join(key.session.folder_name())
            .join(key.block_number.to_string())
            .join(format_block_hash(key.block_hash))
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
        let mut session_entries =
            tokio::fs::read_dir(&self.root_path)
                .await
                .with_context(|| {
                    format!(
                        "failed to read replay archive root {}",
                        self.root_path.display()
                    )
                })?;

        while let Some(session_entry) = session_entries.next_entry().await.with_context(|| {
            format!(
                "failed to read replay archive root entry {}",
                self.root_path.display()
            )
        })? {
            if !session_entry.file_type().await?.is_dir() {
                continue;
            }
            let session = parse_session_entry(&session_entry)?;

            let mut block_entries = tokio::fs::read_dir(session_entry.path())
                .await
                .with_context(|| {
                    format!(
                        "failed to read replay archive session {}",
                        session_entry.path().display()
                    )
                })?;
            while let Some(block_entry) = block_entries.next_entry().await.with_context(|| {
                format!(
                    "failed to read replay archive session entry {}",
                    session_entry.path().display()
                )
            })? {
                if !block_entry.file_type().await?.is_dir() {
                    continue;
                }
                let block_number = parse_block_number_entry(&block_entry)?;

                let mut object_entries = tokio::fs::read_dir(block_entry.path())
                    .await
                    .with_context(|| {
                        format!(
                            "failed to read replay archive block directory {}",
                            block_entry.path().display()
                        )
                    })?;
                while let Some(object_entry) =
                    object_entries.next_entry().await.with_context(|| {
                        format!(
                            "failed to read replay archive object entry {}",
                            block_entry.path().display()
                        )
                    })?
                {
                    if !object_entry.file_type().await?.is_file() {
                        continue;
                    }
                    let block_hash = parse_block_hash_entry(&object_entry)?;
                    keys.push(ReplayArchiveKey::new(
                        session.clone(),
                        block_number,
                        block_hash,
                    ));
                }
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

fn parse_session_entry(entry: &tokio::fs::DirEntry) -> anyhow::Result<ReplayArchiveSession> {
    entry
        .file_name()
        .to_str()
        .context("replay archive session path is not valid UTF-8")?
        .parse()
        .with_context(|| {
            format!(
                "failed to parse replay archive session {}",
                entry.path().display()
            )
        })
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

fn parse_block_hash_entry(entry: &tokio::fs::DirEntry) -> anyhow::Result<BlockHash> {
    let file_name = entry.file_name();
    let file_name = file_name
        .to_str()
        .context("replay archive block hash path is not valid UTF-8")?;
    BlockHash::from_str(file_name).with_context(|| {
        format!(
            "failed to parse replay archive block hash {}",
            entry.path().display()
        )
    })
}
