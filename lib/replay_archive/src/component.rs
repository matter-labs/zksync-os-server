use crate::metrics::REPLAY_ARCHIVE_METRICS;
use crate::{REPLAY_ARCHIVE_QUEUE_SIZE, ReplayArchiver};
use alloy::primitives::BlockHash;
use anyhow::Context as _;
use tokio::sync::mpsc;
use zksync_os_storage_api::ReplayRecord;

pub type ReplayArchiveRecord = (BlockHash, ReplayRecord);
pub type ReplayArchiveSender = mpsc::Sender<ReplayArchiveRecord>;

/// Background component that archives replay records from a bounded queue.
///
/// The block applier only waits until a record is accepted into this component's bounded queue. The
/// actual archive write happens here, off the block-application path. If this queue is full,
/// senders apply backpressure until the component catches up.
pub struct ReplayArchiveComponent<Archive> {
    archive: Archive,
    records: mpsc::Receiver<ReplayArchiveRecord>,
}

impl<Archive> ReplayArchiveComponent<Archive>
where
    Archive: ReplayArchiver,
{
    pub fn new(archive: Archive) -> (ReplayArchiveSender, Self) {
        let (sender, records) = mpsc::channel(REPLAY_ARCHIVE_QUEUE_SIZE);
        (sender, Self { archive, records })
    }

    pub async fn run(mut self) -> anyhow::Result<()> {
        while let Some((block_hash, replay_record)) = self.records.recv().await {
            let block_number = replay_record.block_context.block_number;
            tracing::info!("Archiving replay record for block #{block_number}, {block_hash}");
            self.archive
                .append_replay_record(block_hash, replay_record)
                .await
                .with_context(|| {
                    format!("failed to archive replay record for block {block_number}")
                })?;
            REPLAY_ARCHIVE_METRICS
                .last_archived_block_number
                .set(block_number);
        }
        Ok(())
    }
}
