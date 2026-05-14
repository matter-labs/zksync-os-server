use crate::ReplayArchiveSender;
use crate::metrics::REPLAY_ARCHIVE_METRICS;
use alloy::primitives::{BlockNumber, Sealed};
use std::fmt::Debug;
use std::time::Instant;
use zksync_os_interface::types::BlockContext;
use zksync_os_storage_api::{ReadReplay, ReplayRecord, WriteReplay};

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
    async fn write(&self, record: Sealed<ReplayRecord>, override_allowed: bool) -> bool {
        let (replay_record, block_hash) = record.clone().split();
        let written = self.replay.write(record, override_allowed).await;

        if let Some(archive_sender) = &self.archive_sender {
            REPLAY_ARCHIVE_METRICS
                .queue_depth
                .set(replay_archive_queue_depth(archive_sender));
            let started_at = Instant::now();
            let send_result = archive_sender.send((block_hash, replay_record)).await;
            REPLAY_ARCHIVE_METRICS
                .enqueue_latency
                .observe(started_at.elapsed());
            REPLAY_ARCHIVE_METRICS
                .queue_depth
                .set(replay_archive_queue_depth(archive_sender));
            send_result.expect("replay archive component stopped before accepting replay record");
        }

        written
    }
}

fn replay_archive_queue_depth(sender: &ReplayArchiveSender) -> usize {
    sender.max_capacity() - sender.capacity()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::B256;
    use std::sync::{Arc, Mutex};

    #[derive(Debug, Clone, Default)]
    struct TestReplay {
        records: Arc<Mutex<Vec<Sealed<ReplayRecord>>>>,
    }

    impl ReadReplay for TestReplay {
        fn get_context(&self, block_number: BlockNumber) -> Option<BlockContext> {
            self.get_replay_record(block_number)
                .map(|record| record.block_context)
        }

        fn get_replay_record_by_key(
            &self,
            block_number: BlockNumber,
            _db_key: Option<Vec<u8>>,
        ) -> Option<ReplayRecord> {
            self.records
                .lock()
                .unwrap()
                .iter()
                .find(|record| record.block_context.block_number == block_number)
                .map(|record| record.clone().split().0)
        }

        fn latest_record(&self) -> BlockNumber {
            self.records
                .lock()
                .unwrap()
                .last()
                .map(|record| record.block_context.block_number)
                .unwrap_or(0)
        }
    }

    impl WriteReplay for TestReplay {
        async fn write(&self, record: Sealed<ReplayRecord>, _override_allowed: bool) -> bool {
            self.records.lock().unwrap().push(record);
            true
        }
    }

    #[tokio::test]
    async fn write_persists_record_and_enqueues_archive_record() {
        let replay = TestReplay::default();
        let (archive_sender, mut archive_receiver) = tokio::sync::mpsc::channel(1);
        let replay = ReplayArchivingWriteReplay::new(replay, Some(archive_sender));
        let block_hash = B256::with_last_byte(7);
        let replay_record = test_replay_record(3);

        assert!(
            replay
                .write(
                    Sealed::new_unchecked(replay_record.clone(), block_hash),
                    false
                )
                .await
        );

        assert_eq!(replay.get_replay_record(3), Some(replay_record.clone()));
        assert_eq!(
            archive_receiver.try_recv().unwrap(),
            (block_hash, replay_record)
        );
    }

    fn test_replay_record(block_number: BlockNumber) -> ReplayRecord {
        ReplayRecord {
            block_context: BlockContext {
                block_number,
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
