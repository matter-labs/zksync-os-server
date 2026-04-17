use crate::has_block_range_end::HasBlockRangeEnd;
use tokio::sync::mpsc;

/// A wrapper around `tokio::sync::mpsc::UnboundedReceiver<T>` that adds
/// non-consuming peeks while preserving the original `recv` / `try_recv` semantics.
///
/// Semantics:
/// - `recv().await` / `try_recv()` first drain the internal buffer (if present),
///   otherwise delegate to the inner receiver.
/// - `peek_with` / `peek_recv` expose a reference to the current head without
///   consuming it, loading one item into the local buffer on demand.
pub struct PeekableReceiver<T> {
    inner: mpsc::UnboundedReceiver<T>,
    buf: std::collections::VecDeque<T>,
}

impl<T> PeekableReceiver<T> {
    pub fn new(rx: mpsc::UnboundedReceiver<T>) -> Self {
        Self {
            inner: rx,
            buf: Default::default(),
        }
    }

    /// Receive the next item, awaiting if necessary.
    pub async fn recv(&mut self) -> Option<T> {
        if let Some(v) = self.buf.pop_front() {
            Some(v)
        } else {
            self.inner.recv().await
        }
    }

    /// Try to receive the next item without blocking.
    pub fn try_recv(&mut self) -> Result<T, mpsc::error::TryRecvError> {
        if let Some(v) = self.buf.pop_front() {
            Ok(v)
        } else {
            self.inner.try_recv()
        }
    }

    /// Receive up to `limit` items. Blocks until at least one is available.
    ///
    /// Drains any locally buffered (peeked) items first, then greedily consumes
    /// additional items from the channel via `try_recv` up to `limit`. If the local
    /// buffer is empty, blocks on `recv` for the first item before draining.
    pub async fn recv_many(&mut self, buf: &mut Vec<T>, limit: usize) -> usize {
        let mut count = 0;
        // Drain local peek buffer first.
        if !self.buf.is_empty() {
            let n = self.buf.len().min(limit);
            buf.extend(self.buf.drain(..n));
            count = n;
        }
        // Block for the first item only if we haven't yielded any yet.
        if count == 0 {
            match self.inner.recv().await {
                None => return 0,
                Some(first) => {
                    buf.push(first);
                    count = 1;
                }
            }
        }
        // Greedily drain the rest without blocking.
        while count < limit {
            match self.inner.try_recv() {
                Ok(item) => {
                    buf.push(item);
                    count += 1;
                }
                Err(_) => break,
            }
        }
        count
    }

    /// Consume the buffered item placed by a prior `peek_recv` / `peek_with` call.
    /// Returns `None` if the buffer is empty.
    pub fn pop_buffer(&mut self) -> Option<T> {
        self.buf.pop_front()
    }

    /// Non-consuming peek: loads one item into local buffer via `try_recv`.
    /// Returns `None` if the channel is currently empty.
    pub fn peek_with<R, F: FnOnce(&T) -> R>(&mut self, f: F) -> Option<R> {
        if self.buf.is_empty() {
            match self.inner.try_recv() {
                Ok(v) => self.buf.push_back(v),
                Err(_) => return None,
            }
        }
        self.buf.front().map(f)
    }

    /// Blocking peek: waits for an item and stores it in the local buffer without consuming it.
    pub async fn peek_recv<R, F: FnOnce(&T) -> R>(&mut self, f: F) -> Option<R> {
        if self.buf.is_empty() {
            match self.inner.recv().await {
                Some(v) => self.buf.push_back(v),
                None => return None,
            }
        }
        self.buf.front().map(f)
    }

    pub fn is_closed(&self) -> bool {
        self.inner.is_closed()
    }

    pub fn close(&mut self) {
        self.inner.close();
    }
}

impl<T: HasBlockRangeEnd> PeekableReceiver<T> {
    /// Receive the next item and immediately record it as picked with the health reporter.
    /// Fires at dequeue time (before any processing), recording the "picked" watermark.
    pub async fn recv_and_record_picked(
        &mut self,
        reporter: &zksync_os_observability::ComponentHealthReporter,
    ) -> Option<T> {
        let item = self.recv().await?;
        reporter.record_picked(item.block_number(), item.block_timestamp());
        Some(item)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channel<T>() -> (mpsc::UnboundedSender<T>, PeekableReceiver<T>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (tx, PeekableReceiver::new(rx))
    }

    #[tokio::test]
    async fn send_and_recv() {
        let (tx, mut rx) = channel::<u32>();
        tx.send(1).unwrap();
        tx.send(2).unwrap();
        assert_eq!(rx.recv().await, Some(1));
        assert_eq!(rx.recv().await, Some(2));
    }

    #[tokio::test]
    async fn try_recv_works() {
        let (tx, mut rx) = channel::<u32>();
        tx.send(42).unwrap();
        let v = rx.try_recv().unwrap();
        assert_eq!(v, 42);
    }

    #[tokio::test]
    async fn recv_many_collects_items() {
        let (tx, mut rx) = channel::<u32>();
        tx.send(1).unwrap();
        tx.send(2).unwrap();
        tx.send(3).unwrap();
        let mut buf = vec![];
        let n = rx.recv_many(&mut buf, 10).await;
        assert_eq!(n, 3);
        assert_eq!(buf, vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn peek_does_not_consume() {
        let (tx, mut rx) = channel::<u32>();
        tx.send(99).unwrap();
        let peeked = rx.peek_with(|v| *v);
        assert_eq!(peeked, Some(99));
        // item is still available
        assert_eq!(rx.recv().await, Some(99));
        // now channel is empty
        drop(tx);
        assert_eq!(rx.recv().await, None);
    }

    #[tokio::test]
    async fn peek_recv_blocks_then_buffers() {
        let (tx, mut rx) = channel::<u32>();
        tx.send(7).unwrap();
        // First peek loads the buffer without consuming.
        let a = rx.peek_recv(|v| *v).await;
        assert_eq!(a, Some(7));
        // Second peek returns the same buffered item (no new recv).
        let b = rx.peek_recv(|v| *v).await;
        assert_eq!(b, Some(7));
        // pop_buffer returns the peeked item.
        assert_eq!(rx.pop_buffer(), Some(7));
        assert_eq!(rx.pop_buffer(), None);
    }

    #[tokio::test]
    async fn peek_recv_returns_none_on_close() {
        let (tx, mut rx) = channel::<u32>();
        drop(tx);
        assert_eq!(rx.peek_recv(|v| *v).await, None);
    }

    #[tokio::test]
    async fn recv_many_drains_buf_and_channel() {
        // Regression test: when the local peek buffer is non-empty, recv_many
        // must drain the buffer AND greedily consume additional items from the
        // channel (up to `limit`) in the same call.
        let (tx, mut rx) = channel::<u32>();
        tx.send(1).unwrap();
        // Peek item 1 into the local buffer.
        assert_eq!(rx.peek_with(|v| *v), Some(1));
        // Push two more items into the underlying channel.
        tx.send(2).unwrap();
        tx.send(3).unwrap();
        let mut buf = vec![];
        let n = rx.recv_many(&mut buf, 10).await;
        assert_eq!(n, 3);
        assert_eq!(buf, vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn recv_many_respects_limit_with_peeked_buffer() {
        let (tx, mut rx) = channel::<u32>();
        tx.send(1).unwrap();
        tx.send(2).unwrap();
        assert_eq!(rx.peek_with(|v| *v), Some(1));
        tx.send(3).unwrap();
        let mut buf = vec![];
        // limit = 2 should stop after emitting 2 items (from buf + channel).
        let n = rx.recv_many(&mut buf, 2).await;
        assert_eq!(n, 2);
        assert_eq!(buf, vec![1, 2]);
        // Remaining item is still recoverable.
        assert_eq!(rx.recv().await, Some(3));
    }

    #[tokio::test]
    async fn close_and_is_closed() {
        let (tx, mut rx) = channel::<u32>();
        assert!(!rx.is_closed());
        tx.send(5).unwrap();
        rx.close();
        // Closing the receiver signals no more sends will succeed.
        assert!(tx.send(6).is_err());
        // Items already queued before close are still received.
        assert_eq!(rx.recv().await, Some(5));
        assert_eq!(rx.recv().await, None);
        assert!(rx.is_closed());
    }

    #[tokio::test]
    async fn recv_and_record_picked_calls_reporter() {
        use crate::has_block_range_end::HasBlockRangeEnd;
        use zksync_os_observability::ComponentHealthReporter;

        struct Msg {
            seq: u64,
            ts: u64,
        }
        impl HasBlockRangeEnd for Msg {
            fn block_number(&self) -> u64 {
                self.seq
            }
            fn block_timestamp(&self) -> Option<u64> {
                Some(self.ts)
            }
        }

        let (tx, mut rx) = channel::<Msg>();
        tx.send(Msg { seq: 10, ts: 1000 }).unwrap();

        let (reporter, health_rx) = ComponentHealthReporter::new("test");
        let item = rx.recv_and_record_picked(&reporter).await.unwrap();
        assert_eq!(item.seq, 10);
        assert_eq!(
            health_rx
                .borrow()
                .last_picked
                .as_ref()
                .map(|c| c.block_number),
            Some(10)
        );
        assert_eq!(
            health_rx
                .borrow()
                .last_picked
                .as_ref()
                .and_then(|c| c.timestamp),
            Some(1000)
        );
    }
}
