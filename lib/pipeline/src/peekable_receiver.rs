use crate::has_block_range_end::HasBlockRangeEnd;
use tokio::sync::mpsc;

/// A wrapper around `tokio::sync::mpsc::Receiver<T>` that adds
/// non-consuming peeks while preserving the original receiver semantics.
///
/// Consuming ops (`recv`, `try_recv`, `recv_many`) first drain any item previously
/// placed in the local peek buffer, then delegate to the inner channel.
/// Peek ops (`peek_with`, `peek_recv`) expose a reference to the current head
/// without consuming it, loading one item into the buffer on demand; the buffered
/// item is released by the next consuming call or via `pop_buffer`.
/// `close` / `is_closed` forward to the inner receiver.
pub struct PeekableReceiver<T> {
    inner: mpsc::Receiver<T>,
    buf: std::collections::VecDeque<T>,
}

impl<T> PeekableReceiver<T> {
    pub fn new(rx: mpsc::Receiver<T>) -> Self {
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
        if !self.buf.is_empty() {
            let n = self.buf.len().min(limit);
            buf.extend(self.buf.drain(..n));
            count = n;
        }
        if count == 0 {
            match self.inner.recv().await {
                None => return 0,
                Some(first) => {
                    buf.push(first);
                    count = 1;
                }
            }
        }
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
    /// Receive the next item and immediately record it as picked with the state reporter.
    /// Fires at dequeue time (before any processing), recording the "picked" watermark.
    pub async fn recv_and_record_picked(
        &mut self,
        reporter: &zksync_os_observability::ComponentStateReporter,
    ) -> Option<T> {
        let item = self.recv().await?;
        reporter.record_picked(
            item.block_number(),
            item.block_timestamp(),
            item.batch_number(),
        );
        Some(item)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channel<T>() -> (mpsc::Sender<T>, PeekableReceiver<T>) {
        let (tx, rx) = mpsc::channel(128);
        (tx, PeekableReceiver::new(rx))
    }

    #[tokio::test]
    async fn recv_many_collects_items() {
        let (tx, mut rx) = channel::<u32>();
        tx.try_send(1).unwrap();
        tx.try_send(2).unwrap();
        tx.try_send(3).unwrap();
        let mut buf = vec![];
        let n = rx.recv_many(&mut buf, 10).await;
        assert_eq!(n, 3);
        assert_eq!(buf, vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn peek_does_not_consume() {
        let (tx, mut rx) = channel::<u32>();
        tx.try_send(99).unwrap();
        let peeked = rx.peek_with(|v| *v);
        assert_eq!(peeked, Some(99));
        assert_eq!(rx.recv().await, Some(99));
        drop(tx);
        assert_eq!(rx.recv().await, None);
    }

    #[tokio::test]
    async fn peek_recv_blocks_then_buffers() {
        let (tx, mut rx) = channel::<u32>();
        tx.try_send(7).unwrap();
        let a = rx.peek_recv(|v| *v).await;
        assert_eq!(a, Some(7));
        let b = rx.peek_recv(|v| *v).await;
        assert_eq!(b, Some(7));
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
        // When the local peek buffer is non-empty, recv_many must drain the buffer
        // AND greedily consume additional items from the channel (up to `limit`)
        // in the same call.
        let (tx, mut rx) = channel::<u32>();
        tx.try_send(1).unwrap();
        assert_eq!(rx.peek_with(|v| *v), Some(1));
        tx.try_send(2).unwrap();
        tx.try_send(3).unwrap();
        let mut buf = vec![];
        let n = rx.recv_many(&mut buf, 10).await;
        assert_eq!(n, 3);
        assert_eq!(buf, vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn recv_many_respects_limit_with_peeked_buffer() {
        let (tx, mut rx) = channel::<u32>();
        tx.try_send(1).unwrap();
        tx.try_send(2).unwrap();
        assert_eq!(rx.peek_with(|v| *v), Some(1));
        tx.try_send(3).unwrap();
        let mut buf = vec![];
        let n = rx.recv_many(&mut buf, 2).await;
        assert_eq!(n, 2);
        assert_eq!(buf, vec![1, 2]);
        assert_eq!(rx.recv().await, Some(3));
    }

    #[tokio::test]
    async fn close_and_is_closed() {
        let (tx, mut rx) = channel::<u32>();
        assert!(!rx.is_closed());
        tx.try_send(5).unwrap();
        rx.close();
        assert!(tx.try_send(6).is_err());
        assert_eq!(rx.recv().await, Some(5));
        assert_eq!(rx.recv().await, None);
        assert!(rx.is_closed());
    }

    #[tokio::test]
    async fn recv_and_record_picked_calls_reporter() {
        use crate::has_block_range_end::HasBlockRangeEnd;
        use zksync_os_observability::ComponentStateReporter;

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
        tx.try_send(Msg { seq: 10, ts: 1000 }).unwrap();

        let (reporter, state_rx) = ComponentStateReporter::new("test");
        let item = rx.recv_and_record_picked(&reporter).await.unwrap();
        assert_eq!(item.seq, 10);
        assert_eq!(
            state_rx.borrow().picked.as_ref().map(|c| c.block_number),
            Some(10)
        );
        assert_eq!(
            state_rx.borrow().picked.as_ref().and_then(|c| c.timestamp),
            Some(1000)
        );
    }
}
