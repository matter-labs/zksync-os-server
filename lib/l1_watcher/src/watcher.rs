use crate::metrics::METRICS;
use crate::{BlockBoundary, BlockUpdates, L1WatcherConfig, LogsCache, ProcessRawEvents};
use alloy::primitives::{Address, BlockNumber};
use alloy::providers::Provider;
use alloy::rpc::types::{Filter, Log, ValueOrArray};
use futures::future::BoxFuture;
use tokio::sync::watch;
use zksync_os_provider::NodeProvider;

/// Resolves a watcher's starting state once the starting point `S` is finally known.
///
/// Construction of a watcher only requires its static dependencies; the provider-dependent
/// binary search that turns a starting point (a priority id, batch number, protocol version, …)
/// into a concrete `next_block` — together with the processor that consumes the resolved start
/// point — is deferred into this closure and invoked lazily by [`L1Watcher::run`].
pub(crate) type StartResolver<S> = Box<
    dyn FnOnce(S) -> BoxFuture<'static, anyhow::Result<(BlockNumber, Box<dyn ProcessRawEvents>)>>
        + Send,
>;

/// Builds a [`StartResolver`] from an async closure, hiding the `Box::new`/`Box::pin` ceremony.
pub(crate) fn resolver<S, Fut>(f: impl FnOnce(S) -> Fut + Send + 'static) -> StartResolver<S>
where
    Fut: std::future::Future<Output = anyhow::Result<(BlockNumber, Box<dyn ProcessRawEvents>)>>
        + Send
        + 'static,
{
    Box::new(move |s| Box::pin(f(s)))
}

/// An abstract, *unstarted* watcher for events.
///
/// Holds only the static dependencies needed to poll for blocks and extract logs; the starting
/// point is supplied later to [`run`](Self::run), which resolves it (via [`StartResolver`]) into
/// a concrete start block plus processor and then runs the poll loop. This lets watchers be
/// created in one place and started in another, once the first replayed block is known.
pub struct L1Watcher<S> {
    provider: NodeProvider,
    logs_cache: LogsCache,
    address: ValueOrArray<Address>,
    /// `Some(eb)` makes the watcher exit once the cursor passes `eb`. `None` runs forever.
    end_block: Option<BlockNumber>,
    max_blocks_to_process: u64,
    block_boundary: BlockBoundary,
    block_updates: watch::Receiver<BlockUpdates>,
    resolve_start: StartResolver<S>,
}

impl<S> L1Watcher<S> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn new(
        config: L1WatcherConfig,
        provider: NodeProvider,
        logs_cache: LogsCache,
        block_updates: watch::Receiver<BlockUpdates>,
        address: ValueOrArray<Address>,
        end_block: Option<BlockNumber>,
        l1_chain_id: u64,
        resolve_start: StartResolver<S>,
    ) -> anyhow::Result<Self> {
        let confirmations = if provider.get_chain_id().await? != l1_chain_id {
            // Gateway case, zero out confirmations.
            0
        } else {
            config.confirmations
        };

        Ok(Self {
            provider,
            logs_cache,
            address,
            end_block,
            max_blocks_to_process: config.max_blocks_to_process,
            block_boundary: BlockBoundary::Confirmed { confirmations },
            block_updates,
            resolve_start,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_finalized(
        config: L1WatcherConfig,
        provider: NodeProvider,
        logs_cache: LogsCache,
        block_updates: watch::Receiver<BlockUpdates>,
        address: ValueOrArray<Address>,
        end_block: Option<BlockNumber>,
        resolve_start: StartResolver<S>,
    ) -> Self {
        Self {
            provider,
            logs_cache,
            address,
            end_block,
            max_blocks_to_process: config.max_blocks_to_process,
            block_boundary: BlockBoundary::Finalized,
            block_updates,
            resolve_start,
        }
    }

    /// Resolves the starting point into a concrete start block and processor, then polls for
    /// new events.
    ///
    /// For unbounded watchers (`end_block = None`) this never returns; for bounded watchers it
    /// returns once the cursor passes `end_block`. A failure to resolve the start block is fatal
    /// (panics), matching the previous behavior where resolution happened at construction.
    pub async fn run(self, start: S)
    where
        S: Send + 'static,
    {
        let Self {
            provider,
            logs_cache,
            address,
            end_block,
            max_blocks_to_process,
            block_boundary,
            block_updates,
            resolve_start,
        } = self;
        let (next_block, processor) = resolve_start(start)
            .await
            .expect("failed to resolve L1 watcher start block");
        let mut running = RunningL1Watcher {
            provider,
            logs_cache,
            address,
            next_block,
            end_block,
            max_blocks_to_process,
            block_boundary,
            block_updates,
            processor,
        };
        running.run_inner().await;
    }
}

/// Running state of a watcher whose starting point has already been resolved into a concrete
/// `next_block` and processor. Owns the poll loop; produced by [`L1Watcher::run`] and used
/// directly by [`SlAwareL1Watcher`](crate::SlAwareL1Watcher) to scan a single pre-resolved
/// segment.
pub(crate) struct RunningL1Watcher {
    provider: NodeProvider,
    logs_cache: LogsCache,
    address: ValueOrArray<Address>,
    next_block: BlockNumber,
    /// `Some(eb)` makes the watcher exit `run_inner` once `next_block > eb`. `None` runs forever.
    end_block: Option<BlockNumber>,
    max_blocks_to_process: u64,
    block_boundary: BlockBoundary,
    block_updates: watch::Receiver<BlockUpdates>,
    pub(crate) processor: Box<dyn ProcessRawEvents>,
}

impl RunningL1Watcher {
    /// Builds a running watcher for a single pre-resolved segment, tailing the finalized boundary
    /// (closed segments are dominated by `end_block`, so the boundary mode only matters for the
    /// open-ended segment).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_finalized(
        config: L1WatcherConfig,
        provider: NodeProvider,
        logs_cache: LogsCache,
        block_updates: watch::Receiver<BlockUpdates>,
        address: ValueOrArray<Address>,
        next_block: BlockNumber,
        end_block: Option<BlockNumber>,
        processor: Box<dyn ProcessRawEvents>,
    ) -> Self {
        Self {
            provider,
            logs_cache,
            address,
            next_block,
            end_block,
            max_blocks_to_process: config.max_blocks_to_process,
            block_boundary: BlockBoundary::Finalized,
            block_updates,
            processor,
        }
    }

    /// Runs the poll loop, intended for internal usage in this crate.
    pub(crate) async fn run_inner(&mut self) {
        loop {
            if let Err(e) = self.poll().await {
                tracing::error!("l1 watcher fatal error: {e}");
                panic!("watcher failed: {e}");
            }
            if let Some(eb) = self.end_block
                && self.next_block > eb
            {
                return;
            }
            if let Err(e) = self.block_updates.changed().await {
                tracing::error!("l1 watcher block update channel closed: {e}");
                panic!("l1 watcher block update channel closed: {e}");
            }
        }
    }

    async fn poll(&mut self) -> Result<(), L1WatcherError> {
        let cap = match self.end_block {
            // Closed segment: `end_block` was already resolved against a finalized/executed batch,
            // so the confirmation/finalization window doesn't apply and we don't need an
            // additional RPC.
            Some(eb) => eb,
            None => self
                .block_updates
                .borrow()
                .get_block_number(self.block_boundary),
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
        let new_logs = self.logs_cache.get_logs(&filter).await?;

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
