use futures::{Stream, StreamExt};
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, ready};
use tokio::sync::{Notify, RwLock, mpsc};
use tokio_stream::wrappers::ReceiverStream;
use zksync_os_types::{SystemTxEnvelope, SystemTxType, ZkTransaction};

#[derive(Clone)]
pub struct SlChainIdSubpool {
    notify: Arc<Notify>,
    inner: Arc<RwLock<Inner>>,
}

/// New txs are added to `Inner` as well as it's used to create `SlChainIdTransactionsStream`.
/// `sender` is used to submit new transactions to the active stream.
/// If there is no active stream, then sender will be dropped on the next access; tx is inserted to `pending_txs` anyway.
struct Inner {
    sender: Option<mpsc::Sender<SystemTxEnvelope>>,
    pending_txs: VecDeque<SystemTxEnvelope>,
}

impl Default for SlChainIdSubpool {
    fn default() -> Self {
        Self {
            notify: Arc::new(Notify::new()),
            inner: Arc::new(RwLock::new(Inner {
                sender: None,
                pending_txs: VecDeque::new(),
            })),
        }
    }
}

impl SlChainIdSubpool {
    pub async fn best_transactions_stream(&self) -> SlChainIdTransactionsStream {
        // `1` as buffer is enough because `setSLChainId` transactions are rare.
        let (sender, receiver) = mpsc::channel(1);
        let mut inner = self.inner.write().await;
        inner.sender = Some(sender);
        let state = if let Some(pending_tx) = inner.pending_txs.back() {
            StreamState::Pending(pending_tx.clone())
        } else {
            StreamState::Empty(ReceiverStream::new(receiver))
        };
        SlChainIdTransactionsStream { state }
    }

    pub async fn insert(&self, tx: SystemTxEnvelope) {
        assert!(
            matches!(tx.system_subtype(), SystemTxType::SetSLChainId(_)),
            "tried to insert unrelated system tx ({:?}) into `SlChainIdSubpool`",
            tx.system_subtype()
        );
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

    async fn pop_wait(&self) -> SystemTxEnvelope {
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

    pub async fn on_canonical_state_change(&self, txs: Vec<&SystemTxEnvelope>) -> Option<u64> {
        if txs.is_empty() {
            return None;
        }

        let mut last_migration_number = None;

        for tx in txs {
            if matches!(tx.system_subtype(), SystemTxType::SetSLChainId(u64::MAX)) {
                // If we received a transaction with migration number `u64::MAX`, it means
                // that this is the transaction we executed along with upgrade, so it is not present in the subpool and we should not expect it from the stream.
                // The migration number should not be updated then, so we need to just skip the transaction.
                continue;
            }

            let pending_tx = self.pop_wait().await;
            assert_eq!(tx, &pending_tx);

            if let SystemTxType::SetSLChainId(migration_number) = *tx.system_subtype() {
                last_migration_number = Some(migration_number);
            }
        }
        last_migration_number
    }
}

pub struct SlChainIdTransactionsStream {
    state: StreamState,
}

/// State machine to ensure we serve up to one `setSLChainId` transaction.
enum StreamState {
    /// No discovered `setSLChainId` transaction yet, streaming from gateway migration watcher subscription.
    Empty(ReceiverStream<SystemTxEnvelope>),
    /// `setSLChainId` transaction has been previously discovered.
    Pending(SystemTxEnvelope),
    /// Stream is closed because either one transaction was already returned or upstream receiver was
    /// closed prematurely.
    Closed,
}

impl Stream for SlChainIdTransactionsStream {
    type Item = ZkTransaction;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.as_mut();
        match &mut this.state {
            StreamState::Empty(receiver) => {
                let Some(tx) = ready!(receiver.poll_next_unpin(cx)) else {
                    tracing::debug!("gateway migration watcher stream is closed");
                    this.state = StreamState::Closed;
                    return Poll::Ready(None);
                };
                this.state = StreamState::Closed;
                Poll::Ready(Some(tx.into()))
            }
            StreamState::Pending(tx) => {
                let tx = tx.clone();
                this.state = StreamState::Closed;
                Poll::Ready(Some(tx.into()))
            }
            StreamState::Closed => Poll::Ready(None),
        }
    }
}
