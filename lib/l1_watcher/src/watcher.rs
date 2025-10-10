use crate::metrics::METRICS;
use alloy::primitives::BlockNumber;
use alloy::providers::{DynProvider, Provider};
use alloy::rpc::types::Filter;
use alloy::sol_types::SolEvent;
use std::marker::PhantomData;
use zksync_os_contract_interface::ZkChain;

pub(crate) struct L1Watcher<Event> {
    zk_chain: ZkChain<DynProvider>,
    next_l1_block: BlockNumber,
    max_blocks_to_process: u64,
    _event: PhantomData<Event>,
}

impl<Event> L1Watcher<Event> {
    pub(crate) fn new(
        zk_chain: ZkChain<DynProvider>,
        next_l1_block: BlockNumber,
        max_blocks_to_process: u64,
    ) -> Self {
        Self {
            zk_chain,
            next_l1_block,
            max_blocks_to_process,
            _event: PhantomData,
        }
    }
}

impl<Event: WatchedEvent> L1Watcher<Event> {
    /// Scans up to `self.max_blocks_to_process` next L1 blocks for new events of type `Event`
    /// and returns them.
    pub(crate) async fn poll(&mut self) -> Result<Vec<Event>, L1WatcherError<Event::Error>> {
        let latest_block = self.zk_chain.provider().get_block_number().await?;
        let from_block = self.next_l1_block;
        // Inspect up to `self.max_blocks_to_process` blocks at a time
        let to_block = latest_block.min(from_block + self.max_blocks_to_process - 1);
        if from_block > to_block {
            return Ok(vec![]);
        }
        let new_events = self.process_l1_blocks(from_block, to_block).await?;
        METRICS.events_loaded[&Event::NAME].inc_by(new_events.len() as u64);
        METRICS.most_recently_scanned_l1_block[&Event::NAME].set(to_block);

        self.next_l1_block = to_block + 1;
        Ok(new_events)
    }

    /// Processes a range of L1 blocks for new events.
    ///
    /// Returns a list of new events as extracted from the L1 blocks.
    async fn process_l1_blocks(
        &self,
        from: BlockNumber,
        to: BlockNumber,
    ) -> Result<Vec<Event>, L1WatcherError<Event::Error>> {
        let filter = Filter::new()
            .from_block(from)
            .to_block(to)
            .event_signature(Event::SolEvent::SIGNATURE_HASH)
            .address(*self.zk_chain.address());
        let new_logs = self.zk_chain.provider().get_logs(&filter).await?;
        let new_events = new_logs
            .into_iter()
            .map(|log| {
                let sol_event = Event::SolEvent::decode_log(&log.inner)?.data;
                Event::try_from(sol_event).map_err(L1WatcherError::Convert)
            })
            .collect::<Result<Vec<_>, _>>()?;

        if new_events.is_empty() {
            tracing::trace!("no new events");
        } else {
            tracing::info!(event_count = new_events.len(), "received new events");
        }

        Ok(new_events)
    }
}

pub trait WatchedEvent: TryFrom<Self::SolEvent> {
    const NAME: &'static str;

    type SolEvent: SolEvent;
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
