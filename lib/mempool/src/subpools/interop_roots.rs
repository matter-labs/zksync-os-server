use futures::{Stream, StreamExt, ready};
use std::{
    collections::VecDeque,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::time::Instant;
use tokio::{
    sync::{RwLock, mpsc},
    time::{Sleep, sleep_until},
};
use tokio_stream::wrappers::ReceiverStream;
use zksync_os_types::{
    IndexedInteropRoot, InteropRoot, InteropRootsLogIndex, SystemTxEnvelope, SystemTxType,
    ZkTransaction,
};

#[derive(Clone)]
pub struct InteropRootsSubpool {
    inner: Arc<RwLock<Inner>>,
    channel_size: usize,
}

impl InteropRootsSubpool {
    pub fn new(interop_roots_per_tx: usize, channel_size: usize) -> Self {
        Self {
            inner: Arc::new(RwLock::new(Inner {
                interop_roots_per_tx,
                sender: None,
                pending_roots: VecDeque::new(),
            })),
            channel_size,
        }
    }
}

impl InteropRootsSubpool {
    pub async fn interop_transactions_with_delay(
        &self,
        next_tx_allowed_after: Instant,
    ) -> InteropRootsTransactionsStream {
        self.inner
            .write()
            .await
            .interop_transactions_with_delay(next_tx_allowed_after, self.channel_size)
    }

    pub async fn add_root(&mut self, root: IndexedInteropRoot) {
        self.inner.write().await.add_root(root).await;
    }

    pub async fn on_canonical_state_change(
        &self,
        txs: Vec<&SystemTxEnvelope>,
    ) -> Option<InteropRootsLogIndex> {
        self.inner.write().await.on_canonical_state_change(txs)
    }
}

/// New root are added to `Inner` as well as it's used to create `InteropRootsTransactionsStream`.
/// `sender` is used to submit new roots to the active stream.
/// If there is no active stream, then sender will be dropped on the next access; root is inserted to `pending_txs` anyway.
#[derive(Clone)]
struct Inner {
    interop_roots_per_tx: usize,
    sender: Option<mpsc::Sender<InteropRoot>>,
    pending_roots: VecDeque<IndexedInteropRoot>,
}

pub struct InteropRootsTransactionsStream {
    receiver: ReceiverStream<InteropRoot>,
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
                Poll::Ready(Some(root)) => {
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
        &mut self,
        next_tx_allowed_after: Instant,
        channel_size: usize,
    ) -> InteropRootsTransactionsStream {
        let (sender, receiver) = mpsc::channel(channel_size);
        self.sender = Some(sender);
        InteropRootsTransactionsStream {
            receiver: ReceiverStream::new(receiver),
            pending_roots: self.pending_roots.iter().map(|r| r.root.clone()).collect(),
            interop_roots_per_tx: self.interop_roots_per_tx,
            sleep: Some(Box::pin(sleep_until(next_tx_allowed_after))),
        }
    }

    async fn add_root(&mut self, root: IndexedInteropRoot) {
        if let Some(sender) = &self.sender {
            // If the receiver has been dropped, we should stop sending transactions and clear the sender to avoid unnecessary work.
            if sender.send(root.root.clone()).await.is_err() {
                self.sender.take();
            }
        }
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
