use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use reth_tasks::Runtime;

use crate::{
    AgeEncryptedReplayArchiver, FileSystemReplayArchiveStorage, ReplayArchiveComponent,
    ReplayArchiveSender, ReplayArchiveStorage, ReplayArchiver, ReplayRecordArchiver,
    S3ReplayArchiveConfig, S3ReplayArchiveStorage,
};
#[cfg(feature = "gcp")]
use crate::{
    ArchiveRecipient, GcpKmsClient, GcpKmsConfig, GcpKmsRecipient, GcsReplayArchiveConfig,
    GcsReplayArchiveStorage,
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
    #[cfg(feature = "gcp")]
    Gcs {
        config: GcsReplayArchiveConfig,
        encryption: ReplayArchiveEncryptionConfig,
    },
}

#[derive(Debug, Clone)]
pub enum ReplayArchiveEncryptionConfig {
    Noop,
    AgeX25519 {
        recipient: String,
    },
    #[cfg(feature = "gcp")]
    GcpKms {
        config: GcpKmsConfig,
    },
}

pub type InitializedReplayArchive = (ReplayArchiveSender, Arc<dyn ReplayArchiver>);

pub async fn init_replay_archive(
    config: ReplayArchiveConfig,
    runtime: &Runtime,
) -> Option<InitializedReplayArchive> {
    if let ReplayArchiveConfig::Noop = &config {
        return None;
    }

    // Identifies this node in forensic object metadata; plays no role in the object layout,
    // so all nodes share one flat archive namespace regardless of identity or restarts.
    let writer_node_id = std::env::var("POD_NAME").unwrap_or_else(|_| "node".to_owned());

    let archive = match &config {
        ReplayArchiveConfig::Noop => unreachable!("already checked for Noop option"),
        ReplayArchiveConfig::FileSystem {
            root_path,
            encryption,
        } => {
            let storage =
                FileSystemReplayArchiveStorage::init(root_path.clone(), writer_node_id.clone())
                    .await
                    .context("failed to initialize filesystem replay archive")
                    .expect("failed to initialize replay archive");
            archive_for_storage(storage, encryption).await
        }
        ReplayArchiveConfig::S3 { config, encryption } => {
            let storage = S3ReplayArchiveStorage::init(config.clone(), writer_node_id.clone())
                .await
                .expect("failed to initialize S3 replay archive");
            archive_for_storage(storage, encryption).await
        }
        #[cfg(feature = "gcp")]
        ReplayArchiveConfig::Gcs { config, encryption } => {
            let storage = GcsReplayArchiveStorage::init(config.clone(), writer_node_id.clone())
                .await
                .expect("failed to initialize GCS replay archive");
            archive_for_storage(storage, encryption).await
        }
    };
    let (sender, component) = ReplayArchiveComponent::new(archive.clone());
    runtime.spawn_critical_task("replay archive", async move {
        component
            .run()
            .await
            .expect("replay archive component failed");
    });
    tracing::info!(writer_node_id, "Replay archive enabled");
    Some((sender, archive))
}

async fn archive_for_storage<Storage>(
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
        #[cfg(feature = "gcp")]
        ReplayArchiveEncryptionConfig::GcpKms { config } => {
            let client = GcpKmsClient::new(config)
                .await
                .expect("failed to initialize GCP KMS client for replay archive encryption");
            let recipient = GcpKmsRecipient::fetch(&client)
                .await
                .expect("failed to fetch GCP KMS public key for replay archive encryption");
            Arc::new(AgeEncryptedReplayArchiver::new(
                storage,
                ArchiveRecipient::GcpKms(recipient),
            ))
        }
    }
}
