use crate::ProcessRawEvents;
use crate::metrics::METRICS;
use alloy::primitives::BlockNumber;
use alloy::providers::{DynProvider, Provider};
use alloy::rpc::types::{Filter, Log};
use std::time::Duration;

/// An abstract watcher for L1 events.
/// Handles polling L1 for new blocks and extracting logs,
/// while delegating the actual event processing to a user-provided processor.
pub struct L1Watcher {
    provider: DynProvider,
    next_l1_block: BlockNumber,
    max_blocks_to_process: u64,
    poll_interval: Duration,
    processor: Box<dyn ProcessRawEvents>,
}

impl L1Watcher {
    pub(crate) fn new(
        provider: DynProvider,
        next_l1_block: BlockNumber,
        max_blocks_to_process: u64,
        poll_interval: Duration,
        processor: Box<dyn ProcessRawEvents>,
    ) -> Self {
        Self {
            provider,
            next_l1_block,
            max_blocks_to_process,
            poll_interval,
            processor,
        }
    }
}

impl L1Watcher {
    pub async fn run(mut self) -> Result<(), L1WatcherError> {
        let mut timer = tokio::time::interval(self.poll_interval);
        loop {
            timer.tick().await;
            self.poll().await?;
        }
    }

    async fn poll(&mut self) -> Result<(), L1WatcherError> {
        let latest_block = self.provider.get_block_number().await?;

        while self.next_l1_block <= latest_block {
            let from_block = self.next_l1_block;
            // Inspect up to `self.max_blocks_to_process` blocks at a time
            let to_block = latest_block.min(from_block + self.max_blocks_to_process - 1);

            let events = self
                .extract_logs_from_l1_blocks(from_block, to_block)
                .await?;
            METRICS.events_loaded[&self.processor.name()].inc_by(events.len() as u64);
            METRICS.most_recently_scanned_l1_block[&self.processor.name()].set(to_block);

            for event in events {
                self.processor.process_raw_event(event).await?;
            }

            self.next_l1_block = to_block + 1;
        }

        Ok(())
    }

    /// Processes a range of L1 blocks for new events.
    ///
    /// Returns a list of new events as extracted from the L1 blocks.
    async fn extract_logs_from_l1_blocks(
        &self,
        from: BlockNumber,
        to: BlockNumber,
    ) -> Result<Vec<Log>, L1WatcherError> {
        let filter = Filter::new()
            .from_block(from)
            .to_block(to)
            .event_signature(self.processor.event_signatures())
            .address(self.processor.contract_addresses());
        let new_logs = self.provider.get_logs(&filter).await?;

        if new_logs.is_empty() {
            tracing::trace!(l1_block_from = from, l1_block_to = to, "no new events");
        } else {
            tracing::info!(
                event_count = new_logs.len(),
                l1_block_from = from,
                l1_block_to = to,
                "received new events"
            );
        }

        Ok(new_logs)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum L1WatcherError {
    #[error("L1 does not have any blocks")]
    NoL1Blocks,
    #[error(transparent)]
    Sol(#[from] alloy::sol_types::Error),
    #[error(transparent)]
    Transport(#[from] alloy::transports::TransportError),
    #[error(transparent)]
    Batch(anyhow::Error),
    #[error(transparent)]
    Convert(anyhow::Error),
    #[error("output has been closed")]
    OutputClosed,
}
