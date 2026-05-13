use crate::{
    ReplayArchiveKey, ReplayArchiveObject, ReplayArchiveObjectStream, ReplayArchiveSession,
    ReplayArchiveStorageReader,
};
use alloy::primitives::{BlockHash, BlockNumber};
use anyhow::Context as _;
use async_trait::async_trait;
use futures::StreamExt as _;
use std::path::{Path, PathBuf};
use std::str::FromStr as _;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

const LIST_OBJECTS_CHANNEL_SIZE: usize = 128;

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
}

#[async_trait]
impl ReplayArchiveStorageReader for FileSystemReplayArchiveReader {
    async fn list_objects(&self) -> ReplayArchiveObjectStream {
        let root_path = self.root_path.clone();
        let (sender, receiver) = mpsc::channel(LIST_OBJECTS_CHANNEL_SIZE);
        tokio::spawn(async move {
            if let Err(err) = list_objects(root_path, sender.clone()).await {
                let _ = sender.send(Err(err)).await;
            }
        });
        ReceiverStream::new(receiver).boxed()
    }
}

async fn list_objects(
    root_path: PathBuf,
    sender: mpsc::Sender<anyhow::Result<ReplayArchiveObject>>,
) -> anyhow::Result<()> {
    let mut session_entries = tokio::fs::read_dir(&root_path)
        .await
        .with_context(|| format!("failed to read replay archive root {}", root_path.display()))?;

    while let Some(session_entry) = session_entries.next_entry().await.with_context(|| {
        format!(
            "failed to read replay archive root entry {}",
            root_path.display()
        )
    })? {
        let session_metadata = session_entry.metadata().await.with_context(|| {
            format!(
                "failed to read replay archive session metadata {}",
                session_entry.path().display()
            )
        })?;
        if !session_metadata.is_dir() {
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
            let block_metadata = block_entry.metadata().await.with_context(|| {
                format!(
                    "failed to read replay archive block directory metadata {}",
                    block_entry.path().display()
                )
            })?;
            if !block_metadata.is_dir() {
                continue;
            }

            let block_number = parse_block_number_entry(&block_entry)?;
            let mut object_entries =
                tokio::fs::read_dir(block_entry.path())
                    .await
                    .with_context(|| {
                        format!(
                            "failed to read replay archive block directory {}",
                            block_entry.path().display()
                        )
                    })?;
            while let Some(object_entry) = object_entries.next_entry().await.with_context(|| {
                format!(
                    "failed to read replay archive object entry {}",
                    block_entry.path().display()
                )
            })? {
                let object_metadata = object_entry.metadata().await.with_context(|| {
                    format!(
                        "failed to read replay archive object metadata {}",
                        object_entry.path().display()
                    )
                })?;
                if !object_metadata.is_file() {
                    continue;
                }

                let block_hash = parse_block_hash_entry(&object_entry)?;
                let key = ReplayArchiveKey::new(session.clone(), block_number, block_hash);
                let bytes = tokio::fs::read(object_entry.path())
                    .await
                    .with_context(|| {
                        format!(
                            "failed to read replay archive object {}",
                            object_entry.path().display()
                        )
                    })?;
                if sender
                    .send(Ok(ReplayArchiveObject { key, bytes }))
                    .await
                    .is_err()
                {
                    return Ok(());
                }
            }
        }
    }

    Ok(())
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
    let block_hash = file_name.strip_prefix("0x").unwrap_or(file_name);
    BlockHash::from_str(block_hash).with_context(|| {
        format!(
            "failed to parse replay archive block hash {}",
            entry.path().display()
        )
    })
}
