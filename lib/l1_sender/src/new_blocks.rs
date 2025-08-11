use alloy::primitives::{BlockNumber, U64};
use alloy::providers::{DynProvider, Provider};
use alloy::rpc::client::{NoParams, PollerBuilder};
use alloy::rpc::types::Block;
use alloy::transports::TransportResult;
use async_stream::stream;
use futures::{Stream, StreamExt};
use std::time::Duration;

/// Maximum number of retries for fetching a block.
const MAX_RETRIES: usize = 3;

/// Poll interval to use for fetching the latest block number.
const POLL_INTERVAL: Duration = Duration::from_millis(1000);

/// Streams new blocks from the provider.
pub(crate) struct NewBlocks {
    provider: DynProvider,
    /// The next block to yield.
    next_yield: BlockNumber,
}

impl NewBlocks {
    pub(crate) fn new(provider: DynProvider, next_yield: BlockNumber) -> Self {
        Self {
            provider,
            next_yield,
        }
    }

    pub(crate) fn into_block_stream(
        mut self,
    ) -> impl Stream<Item = TransportResult<Block>> + 'static {
        let mut numbers_stream =
            PollerBuilder::<NoParams, U64>::new(self.provider.weak_client(), "eth_blockNumber", [])
                .with_poll_interval(POLL_INTERVAL)
                .into_stream()
                .map(|n| n.to());

        let stream = stream! {
        'task: loop {
            let Some(block_number) = numbers_stream.next().await else {
                tracing::debug!("polling stream ended");
                break 'task;
            };
            tracing::trace!(%block_number, "got block number");
            if block_number < self.next_yield {
                tracing::debug!(block_number, self.next_yield, "not advanced yet");
                continue 'task;
            }

            let mut retries = MAX_RETRIES;
            for number in self.next_yield..=block_number {
                tracing::debug!(number, "fetching block");
                let block = match self.provider.get_block_by_number(number.into()).await {
                    Ok(Some(block)) => block,
                    Ok(None) if retries > 0 => {
                        tracing::debug!(number, "failed to fetch block, retrying");
                        retries -= 1;
                        continue;
                    }
                    Err(err) => {
                        tracing::error!(number, %err, "failed to fetch block");
                        yield Err(err);
                        break;
                    }
                    Ok(None) => {
                        tracing::error!(number, "failed to fetch block (doesn't exist)");
                        break;
                    }
                };
                tracing::debug!(number=self.next_yield, "yielding block");
                self.next_yield += 1;
                yield Ok(block);
            }
        }
        };

        stream
    }
}
