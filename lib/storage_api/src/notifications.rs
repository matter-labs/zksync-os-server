use crate::{RepositoryBlock, StoredTxData};
use alloy::primitives::TxHash;
use futures::Stream;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, ready};
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;

#[derive(Debug, Clone)]
pub struct BlockNotification {
    pub block: Arc<RepositoryBlock>,
    pub transactions: Vec<(TxHash, Arc<StoredTxData>)>,
}

/// A type that allows to register block subscriptions.
pub trait SubscribeToBlocks: Send + Sync {
    /// Get notified when a new canonical block was imported.
    fn subscribe_to_blocks(&self) -> broadcast::Receiver<BlockNotification>;

    /// Convenience method to get a stream of [`CanonStateNotification`].
    fn block_stream(&self) -> BlockNotificationStream {
        BlockNotificationStream {
            st: BroadcastStream::new(self.subscribe_to_blocks()),
        }
    }
}

/// A Stream of new blocks.
#[derive(Debug)]
pub struct BlockNotificationStream {
    st: BroadcastStream<BlockNotification>,
}

impl Stream for BlockNotificationStream {
    type Item = BlockNotification;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            let mut this = self.as_mut();
            let st = Pin::new(&mut this.st);
            return match ready!(st.poll_next(cx)) {
                Some(Ok(block)) => Poll::Ready(Some(block)),
                Some(Err(err)) => {
                    tracing::info!(%err, "block notification stream lagging behind");
                    continue;
                }
                None => Poll::Ready(None),
            };
        }
    }
}
