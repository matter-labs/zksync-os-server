//! Variation of [`tokio_stream::adapters::Peekable`] that works with [`TxStream`].

use crate::TxStream;
use futures::Stream;
use pin_project::pin_project;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio_stream::StreamExt;

/// Peekable transaction stream.
#[pin_project]
pub struct PeekableTxStream<S: TxStream> {
    peek: Option<S::Item>,
    #[pin]
    stream: S,
}

impl<T: TxStream> PeekableTxStream<T> {
    pub(crate) fn new(stream: T) -> Self {
        Self { peek: None, stream }
    }

    /// Peek at the next item in the stream.
    pub(crate) async fn peek(&mut self) -> Option<&T::Item>
    where
        T: Unpin,
    {
        if let Some(ref it) = self.peek {
            Some(it)
        } else {
            self.peek = self.next().await;
            self.peek.as_ref()
        }
    }
}

impl<S: TxStream> Stream for PeekableTxStream<S> {
    type Item = S::Item;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.project();
        if let Some(it) = this.peek.take() {
            Poll::Ready(Some(it))
        } else {
            this.stream.poll_next(cx)
        }
    }
}

impl<S: TxStream> TxStream for PeekableTxStream<S> {
    fn mark_last_tx_as_invalid(self: Pin<&mut Self>) {
        if self.peek.is_some() {
            panic!("`peek` is not expected to be called during transaction execution");
        }
        // Since `peek` is empty we can delegate to the underlying stream
        self.project().stream.mark_last_tx_as_invalid()
    }
}
