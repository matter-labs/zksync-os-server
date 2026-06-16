use crate::metrics::METRICS;
use crate::{L1WatcherConfig, ProcessRawEvents};
use alloy::primitives::{Address, BlockNumber};
use alloy::providers::Provider;
use alloy::rpc::types::{Filter, Log, ValueOrArray};
use futures::future::BoxFuture;
use zksync_os_provider::NodeProvider;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BlockBoundary {
    Confirmed { confirmations: BlockNumber },
    Finalized,
}

/// Boxed async closure that turns a starting point `S` into a concrete start block and the
/// processor `P` that consumes it.
type ResolveStartFn<S, P> =
    Box<dyn FnOnce(S) -> BoxFuture<'static, anyhow::Result<(BlockNumber, P)>> + Send + Sync>;

/// Deferred constructor for an [`L1Watcher`]: holds the watcher's static dependencies and turns
/// a starting point `S` into a ready-to-run watcher once that starting point is finally known.
///
/// Constructing a resolver only requires static dependencies; the provider-dependent binary
/// search that turns a starting point (a priority id, batch number, protocol version, …) into a
/// concrete `next_block` — together with the processor `P` that consumes the resolved start
/// point — is deferred into the `resolve_start` closure and invoked by
/// [`resolve`](Self::resolve). This lets watchers be created in one place and started in
/// another, once the first replayed block is known.
pub struct StartResolver<S, P> {
    provider: NodeProvider,
    address: ValueOrArray<Address>,
    /// `Some(eb)` makes the watcher exit once the cursor passes `eb`. `None` runs forever.
    end_block: Option<BlockNumber>,
    max_blocks_to_process: u64,
    block_boundary: BlockBoundary,
    resolve_start: ResolveStartFn<S, P>,
}

impl<S, P: ProcessRawEvents> StartResolver<S, P> {
    pub(crate) async fn new<Fut>(
        config: L1WatcherConfig,
        provider: NodeProvider,
        address: ValueOrArray<Address>,
        end_block: Option<BlockNumber>,
        l1_chain_id: u64,
        resolve_start: impl FnOnce(S) -> Fut + Send + Sync + 'static,
    ) -> anyhow::Result<Self>
    where
        Fut: Future<Output = anyhow::Result<(BlockNumber, P)>> + Send + 'static,
    {
        let confirmations = if provider.get_chain_id().await? != l1_chain_id {
            // Gateway case, zero out confirmations.
            0
        } else {
            config.confirmations
        };

        Ok(Self {
            provider,
            address,
            end_block,
            max_blocks_to_process: config.max_blocks_to_process,
            block_boundary: BlockBoundary::Confirmed { confirmations },
            resolve_start: Box::new(move |start| Box::pin(resolve_start(start))),
        })
    }

    /// Like [`new`](Self::new), but tails the finalized boundary so the produced watcher only
    /// reacts to irreversibly observed events.
    pub(crate) fn new_finalized<Fut>(
        config: L1WatcherConfig,
        provider: NodeProvider,
        address: ValueOrArray<Address>,
        end_block: Option<BlockNumber>,
        resolve_start: impl FnOnce(S) -> Fut + Send + Sync + 'static,
    ) -> Self
    where
        Fut: Future<Output = anyhow::Result<(BlockNumber, P)>> + Send + 'static,
    {
        Self {
            provider,
            address,
            end_block,
            max_blocks_to_process: config.max_blocks_to_process,
            block_boundary: BlockBoundary::Finalized,
            resolve_start: Box::new(move |start| Box::pin(resolve_start(start))),
        }
    }

    /// Resolves the starting point into a concrete start block and processor, producing a
    /// ready-to-run [`L1Watcher`].
    pub async fn resolve(self, start: S) -> anyhow::Result<L1Watcher<P>> {
        let Self {
            provider,
            address,
            end_block,
            max_blocks_to_process,
            block_boundary,
            resolve_start,
        } = self;
        let (next_block, processor) = resolve_start(start).await?;
        Ok(L1Watcher {
            provider,
            address,
            next_block,
            end_block,
            max_blocks_to_process,
            block_boundary,
            processor,
        })
    }

    /// Resolves the starting point and runs the produced watcher. A failure to resolve the
    /// start block is fatal (panics), matching the previous behavior where resolution happened
    /// at construction.
    pub async fn run(self, start: S) {
        self.resolve(start)
            .await
            .expect("failed to resolve L1 watcher start block")
            .run()
            .await;
    }
}

/// An abstract watcher for events.
/// Handles polling for new blocks and extracting logs,
/// while delegating the actual event processing to the processor `P`.
///
/// Produced by [`StartResolver::resolve`] once the starting point has been resolved into a
/// concrete `next_block` and processor. May be run unbounded (live tail) or bounded by
/// `end_block` (used by [`SlAwareL1Watcher`](crate::SlAwareL1Watcher) to scan a closed segment
/// to completion).
pub struct L1Watcher<P> {
    provider: NodeProvider,
    address: ValueOrArray<Address>,
    next_block: BlockNumber,
    /// `Some(eb)` makes the watcher exit `run` once `next_block > eb`. `None` runs forever.
    end_block: Option<BlockNumber>,
    max_blocks_to_process: u64,
    block_boundary: BlockBoundary,
    pub(crate) processor: P,
}

impl<P: ProcessRawEvents> L1Watcher<P> {
    /// Builds a watcher for a single pre-resolved segment, tailing the finalized boundary
    /// (closed segments are dominated by `end_block`, so the boundary mode only matters for the
    /// open-ended segment).
    pub(crate) fn new_finalized(
        config: L1WatcherConfig,
        provider: NodeProvider,
        address: ValueOrArray<Address>,
        next_block: BlockNumber,
        end_block: Option<BlockNumber>,
        processor: P,
    ) -> Self {
        Self {
            provider,
            address,
            next_block,
            end_block,
            max_blocks_to_process: config.max_blocks_to_process,
            block_boundary: BlockBoundary::Finalized,
            processor,
        }
    }

    /// Polls for new events.
    ///
    /// For unbounded watchers (`end_block = None`) this never returns; for bounded watchers
    /// it returns once the cursor passes `end_block`.
    pub async fn run(mut self) {
        self.run_inner().await;
    }

    /// Non-consuming version of `run`, intended for internal usage in this crate.
    pub(crate) async fn run_inner(&mut self) {
        let mut headers = match self.block_boundary {
            BlockBoundary::Confirmed { .. } => self.provider.latest_header_watcher().await,
            BlockBoundary::Finalized => self.provider.finalized_header_watcher().await,
        };

        loop {
            let cap = match self.end_block {
                // Closed segment: `end_block` was already resolved against a finalized/executed
                // batch, so the confirmation/finalization window doesn't apply and we don't need
                // an additional RPC.
                Some(end_block) => end_block,
                None => {
                    let number = headers.borrow_and_update().number;
                    match self.block_boundary {
                        BlockBoundary::Confirmed { confirmations } => {
                            number.saturating_sub(confirmations)
                        }
                        BlockBoundary::Finalized => number,
                    }
                }
            };

            if let Err(e) = self.poll(cap).await {
                tracing::error!("l1 watcher fatal error: {e}");
                panic!("watcher failed: {e}");
            }

            if let Some(end_block) = self.end_block
                && self.next_block > end_block
            {
                return;
            }

            if let Err(e) = headers.changed().await {
                tracing::error!("l1 watcher header watcher closed unexpectedly: {e}");
                panic!("l1 watcher header watcher closed unexpectedly: {e}");
            }
        }
    }

    async fn poll(&mut self, cap: BlockNumber) -> Result<(), L1WatcherError> {
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
