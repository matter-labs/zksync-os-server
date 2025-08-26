use alloy::primitives::{BlockNumber, U64};
use alloy::providers::{DynProvider, Provider};
use alloy::rpc::client::{NoParams, PollerBuilder};
use alloy::rpc::types::Block;
use alloy::transports::{RpcError, TransportErrorKind, TransportResult};
use async_stream::stream;
use futures::{Stream, StreamExt};
use std::time::Duration;

/// Maximum number of retries for fetching a block.
const MAX_RETRIES: usize = 3;

/// Streams new blocks from the provider.
pub(crate) struct NewBlocks {
    provider: DynProvider,
    /// The next block to yield.
    next_yield: BlockNumber,
    /// Poll interval to use for fetching the latest block number.
    poll_interval: Duration,
}

impl NewBlocks {
    pub(crate) fn new(
        provider: DynProvider,
        next_yield: BlockNumber,
        poll_interval: Duration,
    ) -> Self {
        Self {
            provider,
            next_yield,
            poll_interval,
        }
    }

    pub(crate) fn into_block_stream(
        mut self,
    ) -> impl Stream<Item = TransportResult<Block>> + 'static {
        let mut numbers_stream =
            PollerBuilder::<NoParams, U64>::new(self.provider.weak_client(), "eth_blockNumber", [])
                .with_poll_interval(self.poll_interval)
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

            for number in self.next_yield..=block_number {
                tracing::debug!(number, "fetching block");
                match retry_get_block(&self.provider, number).await {
                    Ok(block) => {
                        tracing::debug!(number=self.next_yield, "yielding block");
                        self.next_yield += 1;
                        yield Ok(block);
                    }
                    Err(err) => {
                        yield Err(err);
                        break;
                    }
                }
            }
        }
        };

        stream
    }
}

async fn retry_get_block(provider: &DynProvider, number: BlockNumber) -> TransportResult<Block> {
    let mut retries = MAX_RETRIES;
    loop {
        let block = match provider.get_block_by_number(number.into()).await {
            Ok(Some(block)) => block,
            Err(RpcError::Transport(err)) if retries > 0 => {
                tracing::debug!(number, %err, "failed to fetch block, retrying");
                retries -= 1;
                continue;
            }
            Ok(None) if retries > 0 => {
                tracing::debug!(number, "failed to fetch block (doesn't exist), retrying");
                retries -= 1;
                continue;
            }
            Err(err) => {
                tracing::error!(number, %err, "failed to fetch block");
                return Err(err);
            }
            Ok(None) => {
                tracing::error!(number, "failed to fetch block (doesn't exist)");
                return Err(TransportErrorKind::custom_str("block does not exist"));
            }
        };
        return Ok(block);
    }
}
