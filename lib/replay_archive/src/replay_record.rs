use crate::metrics::REPLAY_ARCHIVE_METRICS;
use crate::{ReplayArchiveStorage, ReplayArchiver};
use alloy::primitives::{BlockHash, BlockNumber};
use async_trait::async_trait;
use zksync_os_storage_api::ReplayRecord;

/// Replay archiver that stores JSON-encoded replay records without encryption.
#[derive(Debug, Clone)]
pub struct ReplayRecordArchiver<Storage> {
    storage: Storage,
}

impl<Storage> ReplayRecordArchiver<Storage> {
    pub fn new(storage: Storage) -> Self {
        Self { storage }
    }

    pub fn storage(&self) -> &Storage {
        &self.storage
    }
}

#[async_trait]
impl<Storage> ReplayArchiver for ReplayRecordArchiver<Storage>
where
    Storage: ReplayArchiveStorage,
{
    async fn append_replay_record(
        &self,
        block_hash: BlockHash,
        replay_record: ReplayRecord,
    ) -> anyhow::Result<()> {
        let block_number = replay_record.block_context.block_number;
        let encoded = encode_replay_record(&replay_record);
        REPLAY_ARCHIVE_METRICS.object_bytes[&"stored"].observe(encoded.len());
        self.storage
            .append_object(block_number, block_hash, encoded)
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

pub(crate) fn encode_replay_record(replay_record: &ReplayRecord) -> Vec<u8> {
    serde_json::to_vec(replay_record).expect("failed to encode replay record")
}
