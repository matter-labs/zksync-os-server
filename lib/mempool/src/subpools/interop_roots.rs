use futures::{Stream, StreamExt, ready};
use std::{
    collections::VecDeque,
    pin::Pin,
    sync::{Arc, RwLock},
    task::{Context, Poll},
};
use tokio::time::Instant;
use tokio::{
    sync::broadcast::{self},
    time::{Sleep, sleep_until},
};
use tokio_stream::wrappers::BroadcastStream;
use zksync_os_types::{
    IndexedInteropRoot, InteropRoot, InteropRootsLogIndex, SystemTxEnvelope, SystemTxType,
    ZkTransaction,
};

#[derive(Clone)]
pub struct InteropRootsSubpool {
    inner: Arc<RwLock<Inner>>,
}

impl InteropRootsSubpool {
    pub fn new(interop_roots_per_tx: usize, buffer_size: usize) -> Self {
        Self {
            inner: Arc::new(RwLock::new(Inner {
                interop_roots_per_tx,
                sender: broadcast::Sender::new(buffer_size),
                pending_roots: VecDeque::new(),
            })),
        }
    }
}

impl InteropRootsSubpool {
    pub fn interop_transactions_with_delay(
        &self,
        next_tx_allowed_after: Instant,
    ) -> InteropRootsTransactionsStream {
        self.inner
            .read()
            .unwrap()
            .interop_transactions_with_delay(next_tx_allowed_after)
    }

    pub fn add_root(&mut self, root: IndexedInteropRoot) {
        self.inner.write().unwrap().add_root(root);
    }

    pub fn on_canonical_state_change(
        &self,
        txs: Vec<&SystemTxEnvelope>,
    ) -> Option<InteropRootsLogIndex> {
        self.inner.write().unwrap().on_canonical_state_change(txs)
    }
}

#[derive(Clone)]
struct Inner {
    interop_roots_per_tx: usize,
    sender: broadcast::Sender<InteropRoot>,
    pending_roots: VecDeque<IndexedInteropRoot>,
}

pub struct InteropRootsTransactionsStream {
    receiver: BroadcastStream<InteropRoot>,
    pending_roots: VecDeque<InteropRoot>,
    interop_roots_per_tx: usize,
    sleep: Option<Pin<Box<Sleep>>>,
}

impl Stream for InteropRootsTransactionsStream {
    type Item = ZkTransaction;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if let Some(sleep) = self.sleep.as_mut() {
            ready!(sleep.as_mut().poll(cx));
            self.sleep = None;
        }

        loop {
            if let Some(envelope) = self.take_tx(false) {
                return Poll::Ready(Some(envelope.into()));
            }

            match self.receiver.poll_next_unpin(cx) {
                Poll::Ready(Some(Ok(root))) => {
                    self.pending_roots.push_front(root);
                    continue;
                }
                Poll::Pending => {
                    if let Some(tx) = self.take_tx(true) {
                        return Poll::Ready(Some(tx.into()));
                    }
                    return Poll::Pending;
                }
                Poll::Ready(_) => return Poll::Ready(None),
            }
        }
    }
}

impl InteropRootsTransactionsStream {
    /// Take a transaction from pending roots(not depending on the amount)
    fn take_tx(&mut self, allowed_to_take_remainder: bool) -> Option<SystemTxEnvelope> {
        if self.pending_roots.is_empty()
            || (self.pending_roots.len() < self.interop_roots_per_tx && !allowed_to_take_remainder)
        {
            None
        } else {
            let amount_of_roots_to_take = self.pending_roots.len().min(self.interop_roots_per_tx);
            let starting_index = self.pending_roots.len() - amount_of_roots_to_take;

            let roots_to_consume = self
                .pending_roots
                .drain(starting_index..)
                .rev() // reversing iterator as last element is the one received earliest
                .collect::<Vec<_>>();

            Some(SystemTxEnvelope::import_interop_roots(roots_to_consume))
        }
    }
}

impl Inner {
    fn interop_transactions_with_delay(
        &self,
        next_tx_allowed_after: Instant,
    ) -> InteropRootsTransactionsStream {
        InteropRootsTransactionsStream {
            receiver: BroadcastStream::new(self.sender.subscribe()),
            pending_roots: self.pending_roots.iter().map(|r| r.root.clone()).collect(),
            interop_roots_per_tx: self.interop_roots_per_tx,
            sleep: Some(Box::pin(sleep_until(next_tx_allowed_after))),
        }
    }

    fn add_root(&mut self, root: IndexedInteropRoot) {
        let _ = self.sender.send(root.root.clone());
        self.pending_roots.push_front(root);
    }

    /// Cleans up the stream and removes all roots that were sent in transactions
    /// Returns the last log index of executed interop root
    fn on_canonical_state_change(
        &mut self,
        txs: Vec<&SystemTxEnvelope>,
    ) -> Option<InteropRootsLogIndex> {
        if txs.is_empty() {
            return None;
        }

        let mut log_index = InteropRootsLogIndex::default();

        for tx in txs {
            let SystemTxType::ImportInteropRoots(roots_count) = *tx.system_subtype() else {
                continue;
            };

            // todo: wait for more if `pending_roots.len() < roots_count`
            let starting_index = self.pending_roots.len() - roots_count as usize;

            let roots = self
                .pending_roots
                .drain(starting_index..)
                .rev()
                .collect::<Vec<_>>();

            let envelope = SystemTxEnvelope::import_interop_roots(
                roots.iter().map(|r| r.root.clone()).collect(),
            );
            log_index = roots.last().unwrap().log_index.clone();

            assert_eq!(&envelope, tx);
        }

        Some(log_index)
    }
}
