use futures::{Stream, StreamExt};
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::task::{Context, Poll, ready};
use tokio::sync::{Notify, broadcast};
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
use zksync_os_types::{SystemTxEnvelope, SystemTxType, ZkTransaction};

#[derive(Clone)]
pub struct SlChainIdSubpool {
    notify: Arc<Notify>,
    inner: Arc<RwLock<Inner>>,
}

struct Inner {
    sender: broadcast::Sender<SystemTxEnvelope>,
    pending_txs: VecDeque<SystemTxEnvelope>,
}

impl Default for SlChainIdSubpool {
    fn default() -> Self {
        Self {
            notify: Arc::new(Notify::new()),
            inner: Arc::new(RwLock::new(Inner {
                sender: broadcast::Sender::new(1),
                pending_txs: VecDeque::new(),
            })),
        }
    }
}

impl SlChainIdSubpool {
    pub fn best_transactions_stream(&self) -> SlChainIdTransactionsStream {
        let inner = self.inner.read().unwrap();
        let state = if let Some(pending_tx) = inner.pending_txs.back() {
            StreamState::Pending(pending_tx.clone())
        } else {
            StreamState::Empty(BroadcastStream::new(inner.sender.subscribe()))
        };
        SlChainIdTransactionsStream { state }
    }

    pub fn insert(&self, tx: SystemTxEnvelope) {
        assert_eq!(
            tx.system_subtype(),
            &SystemTxType::SetSLChainId,
            "tried to insert unrelated system tx ({:?}) into `SlChainIdSubpool`",
            tx.system_subtype()
        );
        let mut inner = self.inner.write().unwrap();
        let _ = inner.sender.send(tx.clone());
        inner.pending_txs.push_front(tx);
        self.notify.notify_waiters();
    }

    async fn pop_wait(&self) -> SystemTxEnvelope {
        loop {
            let notified = self.notify.notified();
            {
                let mut inner = self.inner.write().unwrap();
                if let Some(pending_tx) = inner.pending_txs.pop_back() {
                    return pending_tx;
                }
            }
            notified.await;
        }
    }

    pub async fn on_canonical_state_change(&self, txs: Vec<&SystemTxEnvelope>) {
        if txs.is_empty() {
            return;
        }

        for tx in txs {
            let pending_tx = self.pop_wait().await;
            assert_eq!(tx, &pending_tx);
        }
    }
}

pub struct SlChainIdTransactionsStream {
    state: StreamState,
}

/// State machine to ensure we serve up to one `setSLChainId` transaction.
enum StreamState {
    /// No discovered `setSLChainId` transaction yet, streaming from gateway migration watcher subscription.
    Empty(BroadcastStream<SystemTxEnvelope>),
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
                let Some(result) = ready!(receiver.poll_next_unpin(cx)) else {
                    tracing::debug!("gateway migration watcher stream is closed");
                    this.state = StreamState::Closed;
                    return Poll::Ready(None);
                };
                match result {
                    Ok(tx) => {
                        this.state = StreamState::Closed;
                        Poll::Ready(Some(tx.into()))
                    }
                    Err(BroadcastStreamRecvError::Lagged(count)) => {
                        // Fatal error as we lost at least one gateway migration event
                        panic!("gateway migration receiver lagged by {count} items");
                    }
                }
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
