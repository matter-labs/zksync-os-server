use crate::block_replay_storage::BlockReplayStorage;
use anyhow::Context;
use std::time::Duration;
use tokio::sync::mpsc;
use zksync_os_storage_api::{ReadBatch, ReadFinality};

pub async fn send_executed_and_replayed_batch_numbers<
    BatchStorage: ReadBatch,
    Finality: ReadFinality,
>(
    sender: mpsc::Sender<u64>,
    batch_storage: BatchStorage,
    finality_storage: Finality,
    block_replay_storage: Option<BlockReplayStorage>,
    start_from: u64,
) -> anyhow::Result<()> {
    let mut timer = tokio::time::interval(Duration::from_secs(1));
    let mut cursor = start_from;

    loop {
        timer.tick().await;

        let finality_status = finality_storage.get_finality_status();
        let batch_number = batch_storage
            .get_batch_by_block_number(finality_status.last_executed_block)
            .await?
            .context("Missing batch number for the the executed block")?;
        let prev_cursor = cursor;
        for batch in prev_cursor..=batch_number {
            let last_block_number = batch_storage
                .get_batch_range_by_number(batch)
                .await?
                .context("Missing last block number for the batch")?
                .1;
            if let Some(block_replay_storage) = &block_replay_storage
                && block_replay_storage
                    .latest_block()
                    .is_none_or(|replay_block_number| replay_block_number < last_block_number)
            {
                break;
            }
            sender
                .send(batch)
                .await
                .context("Failed to send executed batch number")?;
            cursor = batch + 1;
        }
    }
}
