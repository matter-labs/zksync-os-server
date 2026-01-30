use std::{
    collections::VecDeque,
    pin::Pin,
    task::{Context, Poll},
};

use futures::Stream;
use tokio::sync::mpsc;
use zksync_os_types::{
    IndexedInteropRoot, IndexedInteropRootsEnvelope, InteropEnvelope, InteropRootsLogIndex,
    InteropTxInput,
};

/// Stream that accumulates interop roots and produces interop transactions
/// It also keeps track of sent roots to be able to return them back to stream
/// in case tx was excluded from the block
pub struct InteropTxStream {
    receiver: mpsc::Receiver<IndexedInteropRoot>,
    pending_roots: VecDeque<IndexedInteropRoot>,
    used_roots: VecDeque<IndexedInteropRoot>,
    interop_roots_per_tx: usize,
}

impl Stream for InteropTxStream {
    type Item = IndexedInteropRootsEnvelope;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            match this.receiver.poll_recv(cx) {
                Poll::Ready(Some(root)) => {
                    if let Some(envelope) = this.add_root_and_try_take_tx(root) {
                        return Poll::Ready(Some(envelope));
                    }
                    continue;
                }
                Poll::Pending => {
                    if let Some(envelope) = this.take_tx() {
                        return Poll::Ready(Some(envelope));
                    }
                    return Poll::Pending;
                }
                Poll::Ready(None) => {
                    return Poll::Ready(None);
                }
            }
        }
    }
}

impl InteropTxStream {
    pub fn new(receiver: mpsc::Receiver<IndexedInteropRoot>, interop_roots_per_tx: usize) -> Self {
        Self {
            receiver,
            pending_roots: VecDeque::new(),
            used_roots: VecDeque::new(),
            interop_roots_per_tx,
        }
    }

    /// Add a new root to pending roots and return transaction if the limit of interop roots per import is reached
    fn add_root_and_try_take_tx(
        &mut self,
        root: IndexedInteropRoot,
    ) -> Option<IndexedInteropRootsEnvelope> {
        self.pending_roots.push_back(root);

        if self.pending_roots.len() >= self.interop_roots_per_tx {
            self.take_tx()
        } else {
            None
        }
    }

    /// Take a transaction from pending roots(not depending on the amount)
    fn take_tx(&mut self) -> Option<IndexedInteropRootsEnvelope> {
        if self.pending_roots.is_empty() {
            None
        } else {
            let amount_of_roots_to_take = self.pending_roots.len().min(self.interop_roots_per_tx);
            let roots_to_consume = self
                .pending_roots
                .drain(..amount_of_roots_to_take)
                .collect::<Vec<_>>();

            let tx = IndexedInteropRootsEnvelope {
                log_index: roots_to_consume.last().unwrap().log_index.clone(),
                envelope: InteropEnvelope::new(InteropTxInput::ImportRoots(
                    roots_to_consume
                        .iter()
                        .map(|r| r.root.clone())
                        .collect::<Vec<_>>(),
                )),
            };

            self.used_roots.extend(roots_to_consume);

            Some(tx)
        }
    }

    /// Take next root in the following order:
    /// - used roots
    /// - pending roots
    /// - receiver
    async fn take_next_root(&mut self) -> Option<IndexedInteropRoot> {
        if let Some(root) = self.used_roots.pop_front() {
            Some(root)
        } else if let Some(root) = self.pending_roots.pop_front() {
            Some(root)
        } else {
            self.receiver.recv().await
        }
    }

    /// Cleans up the stream and removes all roots that were sent in transactions
    /// Returns the last log index of executed interop root
    pub async fn on_canonical_state_change(
        &mut self,
        txs: Vec<InteropEnvelope>,
    ) -> Option<InteropRootsLogIndex> {
        let mut log_index = None;
        for tx in txs {
            let mut roots = Vec::new();
            for _ in 0..tx.interop_roots_count() {
                roots.push(self.take_next_root().await.unwrap());
            }

            let envelope = InteropEnvelope::new(InteropTxInput::ImportRoots(
                roots.iter().map(|r| r.root.clone()).collect(),
            ));
            log_index = Some(roots.last().unwrap().log_index.clone());

            assert_eq!(&envelope, &tx);
        }

        assert!(
            self.pending_roots.is_empty(),
            "Pending roots are expected to be empty when on_canonical_state_change is called"
        );

        // Clear used roots that were left in the buffer and move them to pending.
        std::mem::swap(&mut self.pending_roots, &mut self.used_roots);

        log_index
    }
}
