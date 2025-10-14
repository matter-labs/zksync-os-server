use std::collections::VecDeque;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TryRecvError;

/// A wrapper around `tokio::sync::mpsc::Receiver<T>` that adds non-consuming
/// peeks while preserving the original `recv()` / `try_recv()` semantics.
///
/// Semantics:
/// - `recv().await` / `try_recv()` will first drain the internal buffer (if present),
///   otherwise delegate to the inner receiver.
/// - `peek_with()` exposes a reference to the current head without consuming it:
///     * If no item is buffered, it performs a **non-blocking** `try_recv()` to pull one
///       from the channel and stores it in the buffer.
///     * If the channel is empty, returns `None`.
/// - Multi-peek helpers load additional items into the local buffer using **non-blocking**
///   `try_recv()` calls only; they never await.
#[derive(Debug)]
pub struct PeekableReceiver<T> {
    rx: mpsc::Receiver<T>,
    buf: VecDeque<T>, // local, non-consuming buffer of peeked items
}

#[allow(dead_code)]
impl<T> PeekableReceiver<T> {
    pub fn new(rx: mpsc::Receiver<T>) -> Self {
        Self {
            rx,
            buf: VecDeque::new(),
        }
    }

    /// Prepend items to the buffer
    ///
    /// The prepended items will be consumed first, before any buffered or incoming messages.
    /// This is useful for rescheduling messages at the start of a pipeline.
    pub fn prepend(mut self, items: Vec<T>) -> PeekableReceiver<T> {
        // Insert items at the front of the buffer
        for item in items.into_iter().rev() {
            self.buf.push_front(item);
        }
        self
    }

    /// Receive the next item, awaiting if necessary.
    /// If a buffered item exists, it is returned first.
    pub async fn recv(&mut self) -> Option<T> {
        if let Some(v) = self.buf.pop_front() {
            return Some(v);
        }
        self.rx.recv().await
    }

    /// Receive the next item, awaiting if necessary.
    /// If a buffered item exists, it is returned first.
    pub async fn recv_many(&mut self, buffer: &mut Vec<T>, limit: usize) -> usize {
        if !self.buf.is_empty() {
            // Take up to `limit` items from the inner buffer
            let last = self.buf.len().min(limit);
            buffer.extend(self.buf.drain(..last));
            last
        } else {
            self.rx.recv_many(buffer, limit).await
        }
    }

    /// Peek at the next item **without consuming it**, applying `f` to a reference.
    /// Returns `None` if the channel was closed.
    pub async fn peek_recv<R, F>(&mut self, f: F) -> Option<R>
    where
        F: FnOnce(&T) -> R,
    {
        if self.buf.is_empty() {
            match self.rx.recv().await {
                Some(v) => self.buf.push_back(v),
                None => return None, // Channel closed
            }
        }
        self.buf.front().map(f)
    }

    /// Get the next item from the local buffer
    pub fn pop_buffer(&mut self) -> Option<T> {
        self.buf.pop_front()
    }

    /// Try to receive the next item without waiting.
    /// If a buffered item exists, it is returned first.
    pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
        if let Some(v) = self.buf.pop_front() {
            return Ok(v);
        }
        self.rx.try_recv()
    }

    /// Peek at the next item **without consuming it**, applying `f` to a reference.
    /// Returns `None` if the channel is currently empty.
    pub fn peek_with<R, F>(&mut self, f: F) -> Option<R>
    where
        F: FnOnce(&T) -> R,
    {
        if self.buf.is_empty() {
            match self.rx.try_recv() {
                Ok(v) => self.buf.push_back(v),
                Err(_) => return None, // Empty or Disconnected with no items
            }
        }
        self.buf.front().map(f)
    }

    /// Peek at items **without consuming them**, applying `f` to each reference.
    /// Returns clones of all items for which `f` returns true.
    /// Stops when `f` returns false, channel is empty, disconnected, or `limit` reached.
    ///
    /// Non-blocking: uses only `try_recv()` to extend the local buffer.
    pub fn peek_until<F, R>(&mut self, limit: usize, mut f: F) -> Vec<R>
    where
        F: FnMut(&T) -> Option<R>,
    {
        let mut out = Vec::new();

        // Examine up to `limit` items, filling the local buffer as needed.
        for i in 0..limit {
            // Ensure we have at least (i + 1) items buffered; try to pull one if needed.
            if self.buf.len() <= i {
                match self.rx.try_recv() {
                    Ok(v) => self.buf.push_back(v),
                    Err(_) => break, // channel empty or disconnected; nothing more to peek
                }
            }

            // Safe: we either had it buffered or just pushed it.
            let item_ref = match self.buf.get(i) {
                Some(r) => r,
                None => break,
            };

            match f(item_ref) {
                None => break,
                Some(r) => out.push(r),
            }
        }

        out
    }

    /// Consumes items and returns those for which `f` returns true.
    /// Stops when `f` returns false, channel is empty, disconnected, or `limit` reached.
    ///
    /// Non-blocking: uses only `try_recv()`.
    pub fn try_recv_while<F>(&mut self, limit: usize, mut f: F) -> Vec<T>
    where
        F: FnMut(&T) -> bool,
    {
        let mut out = Vec::new();

        while out.len() < limit {
            // Prefer consuming from local buffer first.
            if let Some(front_ref) = self.buf.front() {
                if f(front_ref) {
                    // Predicate passed; consume from buffer.
                    if let Some(v) = self.buf.pop_front() {
                        out.push(v);
                        continue;
                    }
                } else {
                    // Predicate failed; stop without consuming the failing item.
                    break;
                }
            }

            // Buffer is empty; try to pull directly from the channel.
            match self.rx.try_recv() {
                Ok(v) => {
                    if f(&v) {
                        // Consumed because predicate passed.
                        out.push(v);
                    } else {
                        // Predicate failed: put it back at the *front* so it's still next.
                        self.buf.push_front(v);
                        break;
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }

        out
    }

    /// Returns `true` if the channel is closed and no further messages will arrive.
    /// Note: There still may be buffered items locally.
    pub fn is_closed(&self) -> bool {
        self.rx.is_closed()
    }

    /// Close the receiver (stop accepting new messages).
    pub fn close(&mut self) {
        self.rx.close();
    }

    /// Returns the approximate number of messages in the channel **excluding** the local buffer.
    pub fn len_channel(&self) -> usize {
        self.rx.len()
    }

    /// Returns the number of items in the local buffer.
    pub fn len_buffer(&self) -> usize {
        self.rx.len()
    }

    /// Returns the number of items in the channel **including** the local buffer.
    pub fn len(&self) -> usize {
        self.buf.len() + self.rx.len()
    }

    /// Returns `true` if the channel is currently empty **and** there is no item buffered locally.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty() && self.rx.is_empty()
    }

    /// Convert into the inner receiver, consuming buffered items first
    /// WARNING: panics if there are any buffered items!
    pub fn into_inner(self) -> mpsc::Receiver<T> {
        assert!(
            self.buf.is_empty(),
            "PeekableReceiver::into_inner() called with buffered items"
        );
        self.rx
    }
}
