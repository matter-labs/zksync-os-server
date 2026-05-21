use crate::ReplayArchiveSender;
use crate::metrics::REPLAY_ARCHIVE_METRICS;
use alloy::primitives::{BlockNumber, Sealed};
use anyhow::Context;
use std::fmt::Debug;
use std::time::Instant;
use zksync_os_storage_api::{BlockContext, ReadReplay, ReplayRecord, WriteReplay};

/// [`WriteReplay`] wrapper that writes to replay storage and enqueues records for archiving.
#[derive(Debug, Clone)]
pub struct ReplayArchivingWriteReplay<Replay> {
    replay: Replay,
    archive_sender: Option<ReplayArchiveSender>,
}

impl<Replay> ReplayArchivingWriteReplay<Replay> {
    pub fn new(replay: Replay, archive_sender: Option<ReplayArchiveSender>) -> Self {
        Self {
            replay,
            archive_sender,
        }
    }

    pub fn replay(&self) -> &Replay {
        &self.replay
    }
}

impl<Replay> ReadReplay for ReplayArchivingWriteReplay<Replay>
where
    Replay: ReadReplay,
{
    fn get_context(&self, block_number: BlockNumber) -> Option<BlockContext> {
        self.replay.get_context(block_number)
    }

    fn get_replay_record_by_key(
        &self,
        block_number: BlockNumber,
        db_key: Option<Vec<u8>>,
    ) -> Option<ReplayRecord> {
        self.replay.get_replay_record_by_key(block_number, db_key)
    }

    fn latest_record(&self) -> BlockNumber {
        self.replay.latest_record()
    }
}

impl<Replay> WriteReplay for ReplayArchivingWriteReplay<Replay>
where
    Replay: WriteReplay,
{
    async fn write(
        &self,
        record: Sealed<ReplayRecord>,
        override_allowed: bool,
    ) -> anyhow::Result<bool> {
        let (replay_record, block_hash) = record.clone().split();
        let written = self.replay.write(record, override_allowed).await?;

        if let Some(archive_sender) = &self.archive_sender {
            REPLAY_ARCHIVE_METRICS
                .queue_depth
                .set(replay_archive_queue_depth(archive_sender));
            let started_at = Instant::now();
            archive_sender
                .send((block_hash, replay_record))
                .await
                .context("archive_sender closed")?;
            REPLAY_ARCHIVE_METRICS
                .enqueue_latency
                .observe(started_at.elapsed());
            REPLAY_ARCHIVE_METRICS
                .queue_depth
                .set(replay_archive_queue_depth(archive_sender));
        }

        Ok(written)
    }
}

fn replay_archive_queue_depth(sender: &ReplayArchiveSender) -> usize {
    sender.max_capacity() - sender.capacity()
}
