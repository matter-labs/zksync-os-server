use crate::has_block_seq::HasBlockSeq;
use tokio::sync::mpsc;

/// A sender for an unbounded channel.
pub struct TrackedUnboundedSender<T> {
    inner: mpsc::UnboundedSender<T>,
}

impl<T> TrackedUnboundedSender<T> {
    /// Send an item.
    /// Returns `Err` only if the receiver has been dropped.
    pub fn send(&self, value: T) -> Result<(), mpsc::error::SendError<T>> {
        self.inner.send(value)
    }
}

/// A receiver for an unbounded channel with local peek buffering.
pub struct TrackedUnboundedReceiver<T> {
    inner: mpsc::UnboundedReceiver<T>,
    buf: std::collections::VecDeque<T>,
}

impl<T> TrackedUnboundedReceiver<T> {
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
    /// **Important:** If the local peek buffer is non-empty, this method drains only
    /// the peek buffer (up to `limit`) and returns immediately, even if more items are
    /// available on the channel. Call again to collect additional items. This differs
    /// from `tokio::mpsc::Receiver::recv_many` which always greedily drains the channel
    /// after the first item. `l1_sender` does not call `peek_recv` before `recv_many`,
    /// so this asymmetry is not a problem in practice, but callers should be aware.
    pub async fn recv_many(&mut self, buf: &mut Vec<T>, limit: usize) -> usize {
        // Drain local peek buffer first.
        if !self.buf.is_empty() {
            let n = self.buf.len().min(limit);
            buf.extend(self.buf.drain(..n));
            return n;
        }
        // Block for the first item, then greedily drain without blocking.
        match self.inner.recv().await {
            None => 0,
            Some(first) => {
                buf.push(first);
                let mut count = 1;
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
        }
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

    /// Prepend items to the front of the local buffer (e.g. for rescheduling).
    pub fn prepend(mut self, items: Vec<T>) -> Self {
        for item in items.into_iter().rev() {
            self.buf.push_front(item);
        }
        self
    }

    pub fn is_closed(&self) -> bool {
        self.inner.is_closed()
    }

    pub fn close(&mut self) {
        self.inner.close();
    }

    /// Convert into the inner receiver.
    /// # Panics
    /// Panics if there are buffered (peeked) items that would be lost.
    pub fn into_inner(self) -> mpsc::UnboundedReceiver<T> {
        assert!(
            self.buf.is_empty(),
            "TrackedUnboundedReceiver::into_inner() called with buffered items"
        );
        self.inner
    }
}

impl<T: HasBlockSeq> TrackedUnboundedReceiver<T> {
    /// Receive the next item and immediately record it as processed with the health reporter.
    /// Replaces the manual `let last_block = ...; reporter.record_processed(...)` pattern.
    pub async fn recv_and_record(
        &mut self,
        reporter: &zksync_os_observability::ComponentHealthReporter,
    ) -> Option<T> {
        let item = self.recv().await?;
        reporter.record_processed(item.block_seq(), Some(item.block_timestamp()));
        Some(item)
    }
}

/// Create an unbounded channel pair.
pub fn tracked_unbounded_channel<T>() -> (TrackedUnboundedSender<T>, TrackedUnboundedReceiver<T>) {
    let (tx, rx) = mpsc::unbounded_channel();
    (
        TrackedUnboundedSender { inner: tx },
        TrackedUnboundedReceiver {
            inner: rx,
            buf: Default::default(),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn send_and_recv() {
        let (tx, mut rx) = tracked_unbounded_channel::<u32>();
        tx.send(1).unwrap();
        tx.send(2).unwrap();
        assert_eq!(rx.recv().await, Some(1));
        assert_eq!(rx.recv().await, Some(2));
    }

    #[tokio::test]
    async fn send_returns_error_when_receiver_dropped() {
        let (tx, rx) = tracked_unbounded_channel::<u32>();
        drop(rx);
        assert!(tx.send(1).is_err());
    }

    #[tokio::test]
    async fn try_recv_works() {
        let (tx, mut rx) = tracked_unbounded_channel::<u32>();
        tx.send(42).unwrap();
        let v = rx.try_recv().unwrap();
        assert_eq!(v, 42);
    }

    #[tokio::test]
    async fn recv_many_collects_items() {
        let (tx, mut rx) = tracked_unbounded_channel::<u32>();
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
        let (tx, mut rx) = tracked_unbounded_channel::<u32>();
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
    async fn recv_and_record_calls_reporter() {
        use crate::has_block_seq::HasBlockSeq;
        use zksync_os_observability::ComponentHealthReporter;

        struct Msg {
            seq: u64,
            ts: u64,
        }
        impl HasBlockSeq for Msg {
            fn block_seq(&self) -> u64 {
                self.seq
            }
            fn block_timestamp(&self) -> u64 {
                self.ts
            }
        }

        let (tx, mut rx) = tracked_unbounded_channel::<Msg>();
        tx.send(Msg { seq: 10, ts: 1000 }).unwrap();

        let (reporter, health_rx) = ComponentHealthReporter::new("test");
        let item = rx.recv_and_record(&reporter).await.unwrap();
        assert_eq!(item.seq, 10);
        // Verify reporter updated (last_processed_block_number should be Some(10))
        assert_eq!(health_rx.borrow().last_processed_block_number, Some(10));
        assert_eq!(
            health_rx.borrow().last_processed_block_timestamp,
            Some(1000)
        );
    }
}
