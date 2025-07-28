use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TryRecvError;

/// A wrapper around `tokio::sync::mpsc::Receiver<T>` that adds a non‑consuming
/// `peek_with` while preserving the original `recv()` / `try_recv()` semantics.
///
/// Semantics:
/// - `recv().await` / `try_recv()` will first drain the internal head buffer (if present),
///   otherwise delegate to the inner receiver.
/// - `peek_with()` exposes a reference to the current head without consuming it:
///     * If no head is buffered, it performs a **non‑blocking** `try_recv()` to pull one
///       from the channel and stores it in the head buffer.
///     * If the channel is empty, returns `None`.
/// - `consume_head()` consumes the **buffered head only** (never pulls a fresh item).
#[derive(Debug)]
pub struct PeekableReceiver<T> {
    rx: mpsc::Receiver<T>,
    head: Option<T>,
}

#[allow(dead_code)]
impl<T> PeekableReceiver<T> {
    pub fn new(rx: mpsc::Receiver<T>) -> Self {
        Self { rx, head: None }
    }

    /// Receive the next item, awaiting if necessary.
    /// If a head item exists, it is returned first.
    pub async fn recv(&mut self) -> Option<T> {
        if self.head.is_some() {
            return self.head.take();
        }
        self.rx.recv().await
    }

    /// Try to receive the next item without waiting.
    /// If a head item exists, it is returned first.
    pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
        if let Some(v) = self.head.take() {
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
        if self.head.is_none() {
            if let Ok(v) = self.rx.try_recv() {
                self.head = Some(v);
            } else {
                return None;
            }
        }
        self.head.as_ref().map(f)
    }

    /// Consume the **buffered head** if present (never pulls from the channel).
    pub fn consume_head(&mut self) -> Option<T> {
        self.head.take()
    }

    /// Returns `true` if the channel is closed and no further messages will arrive.
    /// Note: There still may be a head item buffered locally.
    pub fn is_closed(&self) -> bool {
        self.rx.is_closed()
    }

    /// Close the receiver (stop accepting new messages).
    pub fn close(&mut self) {
        self.rx.close();
    }

    /// Returns the approximate number of messages in the channel **excluding** the head buffer.
    pub fn len(&self) -> usize {
        self.rx.len()
    }

    /// Returns `true` if the channel is currently empty **and** there is no head item buffered.
    pub fn is_empty(&self) -> bool {
        self.head.is_none() && self.rx.is_empty()
    }
}
