use std::collections::VecDeque;
use std::task::{Context, Poll};
use tokio::sync::mpsc;
use zksync_os_types::L1PriorityEnvelope;

/// A wrapper around `tokio::sync::mpsc::Receiver<L1PriorityEnvelope>` that allows
/// peeking at received items without consuming them, and draining multiple items at once.
///
/// If using for block production, tx stream is supposed to call `poll_recv_and_keep`
/// and then after block is produced, `drain_and_reset` must be called to remove processed txs from the buffer.
/// Note that it's not necessary that all txs in buffer are processed.
///
/// If using for block replay or block rebuild then only `drain_and_reset` must be called
/// to drain txs from the inner channel directly.
#[derive(Debug)]
pub struct L1TxsChannel {
    inner: mpsc::Receiver<L1PriorityEnvelope>,
    buffer: VecDeque<L1PriorityEnvelope>,
    first_unprocessed_idx_in_buffer: usize,
}

impl L1TxsChannel {
    pub fn new(rx: mpsc::Receiver<L1PriorityEnvelope>) -> Self {
        Self {
            inner: rx,
            buffer: VecDeque::new(),
            first_unprocessed_idx_in_buffer: 0,
        }
    }

    /// Polls the inner receiver for a new L1 priority envelope and keeps it in buffer.
    /// If there are already unprocessed envelopes in the buffer, returns the next one.
    pub fn poll_recv_and_keep(&mut self, cx: &mut Context<'_>) -> Poll<Option<L1PriorityEnvelope>> {
        if self.first_unprocessed_idx_in_buffer == self.buffer.len() {
            match self.inner.poll_recv(cx) {
                Poll::Ready(Some(envelope)) => {
                    self.buffer.push_back(envelope);
                }
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }

        let envelope = self.buffer[self.first_unprocessed_idx_in_buffer].clone();
        self.first_unprocessed_idx_in_buffer += 1;

        Poll::Ready(Some(envelope))
    }

    /// Drains `count` envelopes from the buffer, awaiting new ones from the inner receiver if necessary.
    pub async fn drain_and_reset(&mut self, count: usize) -> Vec<L1PriorityEnvelope> {
        while self.buffer.len() < count {
            match self.inner.recv().await {
                Some(envelope) => self.buffer.push_back(envelope),
                None => panic!("Channel closed while draining L1 transactions"),
            }
        }
        let drained = self.buffer.drain(..count).collect();
        self.first_unprocessed_idx_in_buffer = 0;
        drained
    }
}
