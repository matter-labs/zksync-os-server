use crate::metrics::METRICS;
use alloy::primitives::BlockNumber;
use alloy::providers::{DynProvider, Provider};
use alloy::rpc::types::Filter;
use alloy::sol_types::SolEvent;
use std::time::Duration;
use zksync_os_contract_interface::ZkChain;

pub struct L1Watcher<Processor> {
    zk_chain: ZkChain<DynProvider>,
    next_l1_block: BlockNumber,
    max_blocks_to_process: u64,
    poll_interval: Duration,
    processor: Processor,
}

impl<Processor: ProcessL1Event> L1Watcher<Processor> {
    pub(crate) fn new(
        zk_chain: ZkChain<DynProvider>,
        next_l1_block: BlockNumber,
        max_blocks_to_process: u64,
        poll_interval: Duration,
        processor: Processor,
    ) -> Self {
        Self {
            zk_chain,
            next_l1_block,
            max_blocks_to_process,
            poll_interval,
            processor,
        }
    }
}

impl<Processor: ProcessL1Event> L1Watcher<Processor> {
    pub async fn run(mut self) -> Result<(), L1WatcherError<Processor::Error>> {
        let mut timer = tokio::time::interval(self.poll_interval);
        loop {
            timer.tick().await;
            self.poll().await?;
        }
    }

    async fn poll(&mut self) -> Result<(), L1WatcherError<Processor::Error>> {
        let latest_block = self.zk_chain.provider().get_block_number().await?;

        while self.next_l1_block <= latest_block {
            let from_block = self.next_l1_block;
            // Inspect up to `self.max_blocks_to_process` blocks at a time
            let to_block = latest_block.min(from_block + self.max_blocks_to_process - 1);

            let events = self
                .extract_events_from_l1_blocks(from_block, to_block)
                .await?;
            METRICS.events_loaded[&Processor::NAME].inc_by(events.len() as u64);
            METRICS.most_recently_scanned_l1_block[&Processor::NAME].set(to_block);

            for event in events {
                self.processor.process_event(event).await?;
            }

            self.next_l1_block = to_block + 1;
        }

        Ok(())
    }

    /// Processes a range of L1 blocks for new events.
    ///
    /// Returns a list of new events as extracted from the L1 blocks.
    async fn extract_events_from_l1_blocks(
        &self,
        from: BlockNumber,
        to: BlockNumber,
    ) -> Result<Vec<Processor::WatchedEvent>, L1WatcherError<Processor::Error>> {
        let filter = Filter::new()
            .from_block(from)
            .to_block(to)
            .event_signature(Processor::SolEvent::SIGNATURE_HASH)
            .address(*self.zk_chain.address());
        let new_logs = self.zk_chain.provider().get_logs(&filter).await?;
        let new_events = new_logs
            .into_iter()
            .map(|log| {
                let sol_event = Processor::SolEvent::decode_log(&log.inner)?.data;
                Processor::WatchedEvent::try_from(sol_event).map_err(L1WatcherError::Convert)
            })
            .collect::<Result<Vec<_>, _>>()?;

        if new_events.is_empty() {
            tracing::trace!(l1_block_from = from, l1_block_to = to, "no new events");
        } else {
            tracing::info!(
                event_count = new_events.len(),
                l1_block_from = from,
                l1_block_to = to,
                "received new events"
            );
        }

        Ok(new_events)
    }
}

#[allow(async_fn_in_trait)]
pub trait ProcessL1Event {
    const NAME: &'static str;

    type SolEvent: SolEvent;
    type WatchedEvent: TryFrom<Self::SolEvent, Error = Self::Error>;
    type Error: std::error::Error;

    async fn process_event(
        &mut self,
        event: Self::WatchedEvent,
    ) -> Result<(), L1WatcherError<Self::Error>>;
}

#[derive(Debug, thiserror::Error)]
pub enum L1WatcherError<E> {
    #[error("L1 does not have any blocks")]
    NoL1Blocks,
    #[error(transparent)]
    Sol(#[from] alloy::sol_types::Error),
    #[error(transparent)]
    Transport(#[from] alloy::transports::TransportError),
    #[error(transparent)]
    Batch(#[from] anyhow::Error),
    #[error(transparent)]
    Convert(E),
    #[error("output has been closed")]
    OutputClosed,
}
