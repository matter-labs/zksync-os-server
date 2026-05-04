use crate::metrics::METRICS;
use crate::{L1WatcherConfig, ProcessRawEvents};
use alloy::eips::BlockId;
use alloy::primitives::{Address, BlockNumber};
use alloy::providers::{DynProvider, Provider};
use alloy::rpc::types::{Filter, Log, ValueOrArray};
use std::time::Duration;

/// An abstract watcher for events.
/// Handles polling for new blocks and extracting logs,
/// while delegating the actual event processing to a user-provided processor.
///
/// May be run unbounded (live tail) or bounded by `end_block` (used by
/// [`SlAwareL1Watcher`](crate::SlAwareL1Watcher) to scan a closed segment to completion).
pub struct L1Watcher {
    provider: DynProvider,
    address: ValueOrArray<Address>,
    next_block: BlockNumber,
    /// `Some(eb)` makes the watcher exit `run` once `next_block > eb`. `None` runs forever.
    end_block: Option<BlockNumber>,
    max_blocks_to_process: u64,
    block_boundary: BlockBoundary,
    poll_interval: Duration,
    pub(crate) processor: Box<dyn ProcessRawEvents>,
}

enum BlockBoundary {
    Confirmed { confirmations: BlockNumber },
    Finalized,
}

impl L1Watcher {
    pub(crate) async fn new(
        config: L1WatcherConfig,
        provider: DynProvider,
        address: ValueOrArray<Address>,
        next_block: BlockNumber,
        end_block: Option<BlockNumber>,
        l1_chain_id: u64,
        processor: Box<dyn ProcessRawEvents>,
    ) -> anyhow::Result<Self> {
        let confirmations = if provider.get_chain_id().await? != l1_chain_id {
            // Gateway case, zero out confirmations.
            0
        } else {
            config.confirmations
        };

        Ok(Self {
            provider,
            address,
            next_block,
            end_block,
            max_blocks_to_process: config.max_blocks_to_process,
            block_boundary: BlockBoundary::Confirmed { confirmations },
            poll_interval: config.poll_interval,
            processor,
        })
    }

    pub(crate) fn new_finalized(
        config: L1WatcherConfig,
        provider: DynProvider,
        address: ValueOrArray<Address>,
        next_block: BlockNumber,
        end_block: Option<BlockNumber>,
        processor: Box<dyn ProcessRawEvents>,
    ) -> Self {
        Self {
            provider,
            address,
            next_block,
            end_block,
            max_blocks_to_process: config.max_blocks_to_process,
            block_boundary: BlockBoundary::Finalized,
            poll_interval: config.poll_interval,
            processor,
        }
    }
}

impl L1Watcher {
    /// Polls for new events.
    ///
    /// For unbounded watchers (`end_block = None`) this never returns; for bounded watchers
    /// it returns once the cursor passes `end_block`.
    pub async fn run(mut self) {
        self.run_inner().await;
    }

    /// Non-consuming version of `run`, intended for internal usage in this crate.
    pub(crate) async fn run_inner(&mut self) {
        let mut timer = tokio::time::interval(self.poll_interval);
        loop {
            timer.tick().await;
            if let Err(e) = self.poll().await {
                tracing::error!("l1 watcher fatal error: {e}");
                panic!("watcher failed: {e}");
            }
            if let Some(eb) = self.end_block
                && self.next_block > eb
            {
                return;
            }
        }
    }

    async fn poll(&mut self) -> Result<(), L1WatcherError> {
        let cap = match self.end_block {
            // Closed segment: `end_block` was already resolved against a finalized/executed batch,
            // so the confirmation/finalization window doesn't apply and we don't need an
            // additional RPC.
            Some(eb) => eb,
            None => match self.block_boundary {
                BlockBoundary::Confirmed { confirmations } => self
                    .provider
                    .get_block_number()
                    .await?
                    .saturating_sub(confirmations),
                BlockBoundary::Finalized => {
                    let Some(finalized_block) = self
                        .provider
                        .get_block_number_by_id(BlockId::finalized())
                        .await?
                    else {
                        tracing::debug!(
                            event_name = &self.processor.name(),
                            "no finalized L1 block available yet"
                        );
                        return Ok(());
                    };
                    finalized_block
                }
            },
        };

        while self.next_block <= cap {
            let from_block = self.next_block;
            // Inspect up to `self.max_blocks_to_process` blocks at a time
            let to_block = cap.min(from_block + self.max_blocks_to_process - 1);

            let events = self
                .extract_logs_from_l1_blocks(from_block, to_block)
                .await?;

            let events = self.processor.filter_events(events);

            METRICS.events_loaded[&self.processor.name()].inc_by(events.len() as u64);
            METRICS.most_recently_scanned_l1_block[&self.processor.name()].set(to_block);

            for event in events {
                self.processor
                    .process_raw_event(&self.provider, event)
                    .await?;
            }

            self.next_block = to_block + 1;
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
        let mut filter = Filter::new()
            .from_block(from)
            .to_block(to)
            .event_signature(self.processor.event_signatures())
            .address(self.address.clone());
        if let Some(topic1) = self.processor.topic1_filter() {
            filter = filter.topic1(topic1);
        }
        let new_logs = self.provider.get_logs(&filter).await?;

        if new_logs.is_empty() {
            tracing::trace!(
                event_name = self.processor.name(),
                l1_block_from = from,
                l1_block_to = to,
                "no new events"
            );
        } else {
            tracing::info!(
                event_name = self.processor.name(),
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
    #[error(transparent)]
    Contract(#[from] zksync_os_contract_interface::Error),
    #[error(transparent)]
    Other(anyhow::Error),
    #[error(
        "batch {0} was committed on L1 but not submitted by this session; likely a pending tx from a prior crash"
    )]
    UnexpectedCommit(u64),
    #[error("output has been closed")]
    OutputClosed,
}
