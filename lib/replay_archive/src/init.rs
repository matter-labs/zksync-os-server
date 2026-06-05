use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context as _;
use reth_tasks::Runtime;

use crate::{
    AgeEncryptedReplayArchiver, FileSystemReplayArchiveStorage, ReplayArchiveComponent,
    ReplayArchiveSender, ReplayArchiveSession, ReplayArchiveStorage, ReplayArchiver,
    ReplayRecordArchiver, S3ReplayArchiveConfig, S3ReplayArchiveStorage,
};

#[derive(Debug, Clone)]
pub enum ReplayArchiveConfig {
    Noop,
    FileSystem {
        root_path: PathBuf,
        encryption: ReplayArchiveEncryptionConfig,
    },
    S3 {
        config: S3ReplayArchiveConfig,
        encryption: ReplayArchiveEncryptionConfig,
    },
}

#[derive(Debug, Clone)]
pub enum ReplayArchiveEncryptionConfig {
    Noop,
    AgeX25519 { recipient: String },
}

pub type InitializedReplayArchive = (ReplayArchiveSender, Arc<dyn ReplayArchiver>);

pub async fn init_replay_archive(
    config: ReplayArchiveConfig,
    runtime: &Runtime,
) -> Option<InitializedReplayArchive> {
    if let ReplayArchiveConfig::Noop = &config {
        return None;
    }

    let node_id = std::env::var("POD_NAME").unwrap_or_else(|_| "node".to_owned());
    let session = ReplayArchiveSession::new(current_timestamp_millis(), node_id)
        .expect("failed to create replay archive session");

    let archive = match &config {
        ReplayArchiveConfig::Noop => unreachable!("already checked for Noop option"),
        ReplayArchiveConfig::FileSystem {
            root_path,
            encryption,
        } => {
            let storage = FileSystemReplayArchiveStorage::init(root_path.clone(), session.clone())
                .await
                .with_context(|| format!("failed to create replay archive session {session}"))
                .expect("failed to initialize replay archive");
            archive_for_storage(storage, encryption)
        }
        ReplayArchiveConfig::S3 { config, encryption } => {
            let storage = S3ReplayArchiveStorage::init(config.clone(), session.clone())
                .await
                .with_context(|| format!("failed to create replay archive S3 session {session}"))
                .expect("failed to initialize S3 replay archive");
            archive_for_storage(storage, encryption)
        }
    };
    let (sender, component) = ReplayArchiveComponent::new(archive.clone());
    runtime.spawn_critical_task("replay archive", async move {
        component
            .run()
            .await
            .expect("replay archive component failed");
    });
    tracing::info!("Replay archive enabled, session: {session}");
    Some((sender, archive))
}

fn archive_for_storage<Storage>(
    storage: Storage,
    encryption: &ReplayArchiveEncryptionConfig,
) -> Arc<dyn ReplayArchiver>
where
    Storage: ReplayArchiveStorage,
{
    match encryption {
        ReplayArchiveEncryptionConfig::Noop => Arc::new(ReplayRecordArchiver::new(storage)),
        ReplayArchiveEncryptionConfig::AgeX25519 { recipient } => Arc::new(
            AgeEncryptedReplayArchiver::from_recipient_str(storage, recipient)
                .expect("failed to initialize age X25519 replay archive encryption"),
        ),
    }
}

fn current_timestamp_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis()
        .try_into()
        .expect("system time in millis does not fit into u64")
}
