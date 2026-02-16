use crate::TxStream;
use futures::{Stream, StreamExt};
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::task::{Context, Poll};
use tokio::sync::{Notify, broadcast};
use tokio_stream::wrappers::BroadcastStream;
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
            StreamState::Empty
        };
        SlChainIdTransactionsStream {
            receiver: BroadcastStream::new(inner.sender.subscribe()),
            state,
        }
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
    receiver: BroadcastStream<SystemTxEnvelope>,
    state: StreamState,
}

enum StreamState {
    Empty,
    Pending(SystemTxEnvelope),
    Closed,
}

impl Stream for SlChainIdTransactionsStream {
    type Item = ZkTransaction;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.as_mut();
        match &this.state {
            StreamState::Empty => {}
            StreamState::Pending(tx) => {
                let tx = tx.clone();
                this.state = StreamState::Closed;
                return Poll::Ready(Some(tx.into()));
            }
            StreamState::Closed => {
                return Poll::Ready(None);
            }
        }

        match this.receiver.poll_next_unpin(cx) {
            Poll::Ready(Some(Ok(tx))) => {
                this.state = StreamState::Closed;
                Poll::Ready(Some(tx.into()))
            }
            Poll::Pending => Poll::Pending,
            Poll::Ready(_) => Poll::Ready(None),
        }
    }
}

impl TxStream for SlChainIdTransactionsStream {
    fn mark_last_tx_as_invalid(self: Pin<&mut Self>) {
        panic!("cannot mark setSlChainId transaction as invalid")
    }
}
