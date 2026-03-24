use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::mpsc;

/// A sender for an unbounded channel that tracks the number of items in flight.
/// The depth counter is incremented on every successful `send()` and decremented
/// by the paired `TrackedUnboundedReceiver` on every consumed item.
pub struct TrackedUnboundedSender<T> {
    inner: mpsc::UnboundedSender<T>,
    depth: Arc<AtomicUsize>,
}

impl<T> TrackedUnboundedSender<T> {
    /// Send an item. Increments the depth counter on success.
    /// Returns `Err` only if the receiver has been dropped.
    pub fn send(&self, value: T) -> Result<(), mpsc::error::SendError<T>> {
        match self.inner.send(value) {
            Ok(()) => {
                self.depth.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// Returns a clone of the shared depth counter (live queue length).
    pub fn depth(&self) -> Arc<AtomicUsize> {
        self.depth.clone()
    }
}

/// A receiver for a depth-tracked unbounded channel.
/// Decrements the shared depth counter whenever an item is consumed.
pub struct TrackedUnboundedReceiver<T> {
    inner: mpsc::UnboundedReceiver<T>,
    buf: std::collections::VecDeque<T>,
    depth: Arc<AtomicUsize>,
}

impl<T> TrackedUnboundedReceiver<T> {
    /// Receive the next item, awaiting if necessary.
    pub async fn recv(&mut self) -> Option<T> {
        let item = if let Some(v) = self.buf.pop_front() {
            Some(v)
        } else {
            self.inner.recv().await
        };
        if item.is_some() {
            self.depth.fetch_sub(1, Ordering::Relaxed);
        }
        item
    }

    /// Try to receive the next item without blocking.
    pub fn try_recv(&mut self) -> Result<T, mpsc::error::TryRecvError> {
        let item = if let Some(v) = self.buf.pop_front() {
            Ok(v)
        } else {
            self.inner.try_recv()
        };
        if item.is_ok() {
            self.depth.fetch_sub(1, Ordering::Relaxed);
        }
        item
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
            self.depth.fetch_sub(n, Ordering::Relaxed);
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
                self.depth.fetch_sub(count, Ordering::Relaxed);
                count
            }
        }
    }

    /// Consume the buffered item placed by a prior `peek_recv` / `peek_with` call.
    /// Returns `None` if the buffer is empty.
    /// Decrements the depth counter since the item is now consumed.
    pub fn pop_buffer(&mut self) -> Option<T> {
        let item = self.buf.pop_front();
        if item.is_some() {
            self.depth.fetch_sub(1, Ordering::Relaxed);
        }
        item
    }

    /// Non-consuming peek: loads one item into local buffer via `try_recv`.
    /// Does NOT decrement depth (item is still "in transit" until consumed).
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
    /// Does NOT decrement depth.
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

/// Create a depth-tracked unbounded channel pair.
pub fn tracked_unbounded_channel<T>() -> (TrackedUnboundedSender<T>, TrackedUnboundedReceiver<T>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let depth = Arc::new(AtomicUsize::new(0));
    (
        TrackedUnboundedSender {
            inner: tx,
            depth: depth.clone(),
        },
        TrackedUnboundedReceiver {
            inner: rx,
            buf: Default::default(),
            depth,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    #[tokio::test]
    async fn send_increments_depth() {
        let (tx, mut rx) = tracked_unbounded_channel::<u32>();
        assert_eq!(tx.depth().load(Ordering::SeqCst), 0);
        tx.send(1).unwrap();
        assert_eq!(tx.depth().load(Ordering::SeqCst), 1);
        tx.send(2).unwrap();
        assert_eq!(tx.depth().load(Ordering::SeqCst), 2);
        rx.recv().await;
        assert_eq!(tx.depth().load(Ordering::SeqCst), 1);
        rx.recv().await;
        assert_eq!(tx.depth().load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn send_returns_error_when_receiver_dropped() {
        let (tx, rx) = tracked_unbounded_channel::<u32>();
        drop(rx);
        assert!(tx.send(1).is_err());
    }

    #[tokio::test]
    async fn try_recv_decrements_depth() {
        let (tx, mut rx) = tracked_unbounded_channel::<u32>();
        tx.send(42).unwrap();
        assert_eq!(tx.depth().load(Ordering::SeqCst), 1);
        let v = rx.try_recv().unwrap();
        assert_eq!(v, 42);
        assert_eq!(tx.depth().load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn recv_many_decrements_depth() {
        let (tx, mut rx) = tracked_unbounded_channel::<u32>();
        tx.send(1).unwrap();
        tx.send(2).unwrap();
        tx.send(3).unwrap();
        assert_eq!(tx.depth().load(Ordering::SeqCst), 3);
        let mut buf = vec![];
        let n = rx.recv_many(&mut buf, 10).await;
        assert_eq!(n, 3);
        assert_eq!(buf, vec![1, 2, 3]);
        assert_eq!(tx.depth().load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn peek_does_not_decrement_depth() {
        let (tx, mut rx) = tracked_unbounded_channel::<u32>();
        tx.send(99).unwrap();
        assert_eq!(tx.depth().load(Ordering::SeqCst), 1);
        let peeked = rx.peek_with(|v| *v);
        assert_eq!(peeked, Some(99));
        // depth unchanged — item is buffered locally, not consumed
        assert_eq!(tx.depth().load(Ordering::SeqCst), 1);
        // consuming it decrements
        rx.recv().await;
        assert_eq!(tx.depth().load(Ordering::SeqCst), 0);
    }
}
