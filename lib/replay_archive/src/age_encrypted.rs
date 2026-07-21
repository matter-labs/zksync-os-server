#[cfg(feature = "gcp")]
use crate::kms::GcpKmsRecipient;
use crate::metrics::REPLAY_ARCHIVE_METRICS;
use crate::replay_record::encode_replay_record;
use crate::{ReplayArchiveStorage, ReplayArchiver, ensure_object_archived};
use age_core::format::{FileKey, Stanza};
use alloy::primitives::{BlockHash, BlockNumber};
use anyhow::Context as _;
use async_trait::async_trait;
use std::collections::HashSet;
use std::time::Instant;
use zksync_os_storage_api::ReplayRecord;

const BYTES_PER_MEGABYTE: f64 = 1024.0 * 1024.0;

/// age recipient used for replay archive record encryption.
#[derive(Debug, Clone)]
pub enum ArchiveRecipient {
    X25519(age::x25519::Recipient),
    #[cfg(feature = "gcp")]
    GcpKms(GcpKmsRecipient),
}

impl age::Recipient for ArchiveRecipient {
    fn wrap_file_key(
        &self,
        file_key: &FileKey,
    ) -> Result<(Vec<Stanza>, HashSet<String>), age::EncryptError> {
        match self {
            Self::X25519(recipient) => recipient.wrap_file_key(file_key),
            #[cfg(feature = "gcp")]
            Self::GcpKms(recipient) => recipient.wrap_file_key(file_key),
        }
    }
}

/// Replay archiver that stores age-encrypted JSON replay records.
#[derive(Debug, Clone)]
pub struct AgeEncryptedReplayArchiver<Storage> {
    storage: Storage,
    recipient: ArchiveRecipient,
}

impl<Storage> AgeEncryptedReplayArchiver<Storage> {
    pub fn new(storage: Storage, recipient: ArchiveRecipient) -> Self {
        Self { storage, recipient }
    }

    pub fn from_recipient_str(storage: Storage, recipient: &str) -> anyhow::Result<Self> {
        let recipient = recipient
            .parse()
            .map_err(|err| anyhow::anyhow!("failed to parse age X25519 recipient: {err}"))?;
        Ok(Self::new(storage, ArchiveRecipient::X25519(recipient)))
    }

    pub(crate) fn encrypt_replay_record(
        &self,
        replay_record: &ReplayRecord,
    ) -> anyhow::Result<Vec<u8>> {
        let encoded = encode_replay_record(replay_record);
        let encoded_len = encoded.len();
        REPLAY_ARCHIVE_METRICS.object_bytes[&"plaintext"].observe(encoded_len);

        let started_at = Instant::now();
        let encrypted = age::encrypt(&self.recipient, encoded.as_slice())
            .context("failed to encrypt replay record with age")?;
        let elapsed = started_at.elapsed();
        REPLAY_ARCHIVE_METRICS.encryption_time.observe(elapsed);
        if encoded_len > 0 {
            REPLAY_ARCHIVE_METRICS
                .encryption_time_per_megabyte
                .observe(elapsed.as_secs_f64() * BYTES_PER_MEGABYTE / encoded_len as f64);
        }
        REPLAY_ARCHIVE_METRICS.object_bytes[&"stored"].observe(encrypted.len());
        Ok(encrypted)
    }
}

#[async_trait]
impl<Storage> ReplayArchiver for AgeEncryptedReplayArchiver<Storage>
where
    Storage: ReplayArchiveStorage,
{
    async fn ensure_replay_record(
        &self,
        block_hash: BlockHash,
        replay_record: ReplayRecord,
    ) -> anyhow::Result<()> {
        let block_number = replay_record.block_context.block_number;
        // Encryption is deferred into the encode closure: when the block is already archived
        // (the common case for follower nodes), no encryption work happens at all.
        ensure_object_archived(&self.storage, block_number, block_hash, || {
            self.encrypt_replay_record(&replay_record)
        })
        .await
    }

    async fn contains_replay_record(
        &self,
        block_number: BlockNumber,
        block_hash: BlockHash,
    ) -> anyhow::Result<bool> {
        self.storage.contains_object(block_number, block_hash).await
    }
}
