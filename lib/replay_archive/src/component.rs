use crate::metrics::REPLAY_ARCHIVE_METRICS;
use crate::{REPLAY_ARCHIVE_QUEUE_SIZE, ReplayArchiver};
use alloy::primitives::{BlockHash, BlockNumber};
use anyhow::Context as _;
use futures::stream::{FuturesUnordered, StreamExt};
use tokio::sync::mpsc;
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
        let Self {
            archive,
            mut records,
        } = self;
        let mut in_flight = FuturesUnordered::new();
        let mut records_closed = false;
        let mut highest_archived_block_number = None;

        loop {
            while in_flight.len() < MAX_PARALLEL_OBJECT_PUTS && !records_closed {
                match records.try_recv() {
                    Ok(record) => {
                        in_flight.push(archive_replay_record(&archive, record));
                    }
                    Err(mpsc::error::TryRecvError::Empty) => break,
                    Err(mpsc::error::TryRecvError::Disconnected) => records_closed = true,
                }
            }

            if in_flight.is_empty() {
                if records_closed {
                    break;
                }

                match records.recv().await {
                    Some(record) => {
                        in_flight.push(archive_replay_record(&archive, record));
                    }
                    None => records_closed = true,
                }
                continue;
            }

            let archived_block_number = tokio::select! {
                record = records.recv(), if !records_closed && in_flight.len() < MAX_PARALLEL_OBJECT_PUTS => {
                    match record {
                        Some(record) => {
                            in_flight.push(archive_replay_record(&archive, record));
                            continue;
                        }
                        None => {
                            records_closed = true;
                            continue;
                        }
                    }
                }
                result = in_flight.next() => {
                    result.expect("in-flight archive writes are not empty")?
                }
            };

            if highest_archived_block_number.is_none_or(|highest| archived_block_number > highest) {
                REPLAY_ARCHIVE_METRICS
                    .last_archived_block_number
                    .set(archived_block_number);
                highest_archived_block_number = Some(archived_block_number);
            }
        }
        Ok(())
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
