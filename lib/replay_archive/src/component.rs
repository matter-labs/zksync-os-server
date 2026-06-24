use crate::metrics::REPLAY_ARCHIVE_METRICS;
use crate::{REPLAY_ARCHIVE_QUEUE_SIZE, ReplayArchiver};
use alloy::primitives::{BlockHash, BlockNumber};
use anyhow::Context as _;
use futures::{StreamExt as _, TryStreamExt as _};
use std::sync::Mutex;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use zksync_os_storage_api::ReplayRecord;

pub type ReplayArchiveRecord = (BlockHash, ReplayRecord);
pub type ReplayArchiveSender = mpsc::Sender<ReplayArchiveRecord>;

const MAX_PARALLEL_OBJECT_PUTS: usize = 10;

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

    pub async fn run(self) -> anyhow::Result<()> {
        let Self { archive, records } = self;
        let highest_archived_block_number = Mutex::new(None);

        ReceiverStream::new(records)
            .map(Ok::<_, anyhow::Error>)
            .try_for_each_concurrent(MAX_PARALLEL_OBJECT_PUTS, |record| {
                let archive = &archive;
                let highest_archived_block_number = &highest_archived_block_number;

                async move {
                    let archived_block_number = archive_replay_record(archive, record).await?;
                    update_highest_archived_block_number(
                        highest_archived_block_number,
                        archived_block_number,
                    );
                    Ok(())
                }
            })
            .await
    }
}

fn update_highest_archived_block_number(
    highest_archived_block_number: &Mutex<Option<BlockNumber>>,
    archived_block_number: BlockNumber,
) {
    let mut highest_archived_block_number = highest_archived_block_number
        .lock()
        .expect("highest archived block number mutex is poisoned");

    if highest_archived_block_number.is_none_or(|highest| archived_block_number > highest) {
        REPLAY_ARCHIVE_METRICS
            .last_archived_block_number
            .set(archived_block_number);
        *highest_archived_block_number = Some(archived_block_number);
    }
}

async fn archive_replay_record<Archive>(
    archive: &Archive,
    (block_hash, replay_record): ReplayArchiveRecord,
) -> anyhow::Result<BlockNumber>
where
    Archive: ReplayArchiver,
{
    let block_number = replay_record.block_context.block_number;
    tracing::info!("Archiving replay record for block #{block_number}, {block_hash}");
    archive
        .append_replay_record(block_hash, replay_record)
        .await
        .with_context(|| format!("failed to archive replay record for block {block_number}"))?;
    Ok(block_number)
}
