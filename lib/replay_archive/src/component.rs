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
    let archive_time = REPLAY_ARCHIVE_METRICS.archive_time.start();
    archive
        .ensure_replay_record(block_hash, replay_record)
        .await
        .with_context(|| format!("failed to archive replay record for block {block_number}"))?;
    archive_time.observe();
    Ok(block_number)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::B256;
    use async_trait::async_trait;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use vise::{Format, MetricsCollection};

    #[derive(Debug, Default)]
    struct RecordingArchiver {
        appended: AtomicUsize,
    }

    #[async_trait]
    impl ReplayArchiver for RecordingArchiver {
        async fn ensure_replay_record(
            &self,
            _block_hash: BlockHash,
            _replay_record: ReplayRecord,
        ) -> anyhow::Result<()> {
            self.appended.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn contains_replay_record(
            &self,
            _block_number: BlockNumber,
            _block_hash: BlockHash,
        ) -> anyhow::Result<bool> {
            Ok(false)
        }
    }

    // Relies on nextest's process-per-test isolation: the global metric is not shared with
    // other tests.
    #[tokio::test]
    async fn archiving_observes_archive_time_metric() {
        let archiver = Arc::new(RecordingArchiver::default());
        let (sender, component) = ReplayArchiveComponent::new(archiver.clone());

        sender
            .send((B256::with_last_byte(1), test_replay_record()))
            .await
            .unwrap();
        drop(sender);
        component.run().await.unwrap();

        assert_eq!(archiver.appended.load(Ordering::SeqCst), 1);
        let mut encoded = String::new();
        MetricsCollection::default()
            .collect()
            .encode(&mut encoded, Format::OpenMetrics)
            .unwrap();
        let count_line = encoded
            .lines()
            .find(|line| line.starts_with("replay_archive_archive_time_seconds_count"))
            .unwrap_or_else(|| panic!("archive_time metric is not exported:\n{encoded}"));
        assert!(count_line.ends_with(" 1"), "{count_line}");
    }

    fn test_replay_record() -> ReplayRecord {
        ReplayRecord {
            block_context: zksync_os_storage_api::BlockContext {
                block_number: 7,
                ..Default::default()
            },
            transactions: vec![],
            previous_block_timestamp: 0,
            node_version: "0.0.0".parse().unwrap(),
            protocol_version: "0.29.1".parse().unwrap(),
            block_output_hash: B256::ZERO,
            force_preimages: vec![],
            starting_cursors: Default::default(),
        }
    }
}
