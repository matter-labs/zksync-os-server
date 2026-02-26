use futures::{Stream, StreamExt};
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::sync::{Notify, RwLock, mpsc};
use tokio_stream::wrappers::ReceiverStream;
use zksync_os_types::{L1PriorityEnvelope, L1TxSerialId, ZkTransaction};

#[derive(Clone)]
pub struct L1Subpool {
    notify: Arc<Notify>,
    inner: Arc<RwLock<Inner>>,
    channel_size: usize,
}

/// New txs are added to `Inner` as well as it's used to create `L1TransactionsStream`.
/// `sender` is used to submit new transactions to the active stream.
/// If there is no active stream, then sender will be dropped on the next access; tx is inserted to `pending_txs` anyway.
struct Inner {
    sender: Option<mpsc::Sender<Arc<L1PriorityEnvelope>>>,
    pending_txs: VecDeque<Arc<L1PriorityEnvelope>>,
}

impl L1Subpool {
    pub fn new(channel_size: usize) -> Self {
        Self {
            notify: Arc::new(Notify::new()),
            inner: Arc::new(RwLock::new(Inner {
                sender: None,
                pending_txs: VecDeque::new(),
            })),
            channel_size,
        }
    }

    pub async fn best_transactions_stream(&self) -> L1TransactionsStream {
        let (sender, receiver) = mpsc::channel(self.channel_size);
        let mut inner = self.inner.write().await;
        inner.sender = Some(sender);
        L1TransactionsStream {
            receiver: ReceiverStream::new(receiver),
            pending_txs: inner.pending_txs.clone(),
        }
    }

    pub async fn insert(&mut self, tx: Arc<L1PriorityEnvelope>) {
        let mut inner = self.inner.write().await;
        if let Some(sender) = &inner.sender {
            // If the receiver has been dropped, we should stop sending transactions and clear the sender to avoid unnecessary work.
            if sender.send(tx.clone()).await.is_err() {
                inner.sender.take();
            }
        }
        inner.pending_txs.push_front(tx);
        self.notify.notify_waiters();
    }

    async fn pop_wait(&self) -> Arc<L1PriorityEnvelope> {
        loop {
            let notified = self.notify.notified();
            {
                let mut inner = self.inner.write().await;
                if let Some(pending_tx) = inner.pending_txs.pop_back() {
                    return pending_tx;
                }
            }
            notified.await;
        }
    }

    pub async fn on_canonical_state_change(
        &self,
        txs: Vec<&L1PriorityEnvelope>,
    ) -> Option<L1TxSerialId> {
        if txs.is_empty() {
            return None;
        }

        let mut priority_id = 0;
        for tx in txs {
            let pending_tx = self.pop_wait().await;
            assert_eq!(tx, pending_tx.as_ref());
            priority_id = pending_tx.priority_id();
        }

        Some(priority_id)
    }
}

pub struct L1TransactionsStream {
    receiver: ReceiverStream<Arc<L1PriorityEnvelope>>,
    pending_txs: VecDeque<Arc<L1PriorityEnvelope>>,
}

impl Stream for L1TransactionsStream {
    type Item = ZkTransaction;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if let Some(tx) = self.pending_txs.pop_back() {
            return Poll::Ready(Some(tx.as_ref().clone().into()));
        }

        match self.receiver.poll_next_unpin(cx) {
            Poll::Ready(Some(tx)) => Poll::Ready(Some(tx.as_ref().clone().into())),
            Poll::Pending => Poll::Pending,
            Poll::Ready(_) => Poll::Ready(None),
        }
    }
}
