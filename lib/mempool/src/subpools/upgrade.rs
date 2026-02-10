use crate::TxStream;
use futures::{Stream, StreamExt};
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::task::{Context, Poll};
use tokio::sync::{Notify, broadcast};
use tokio_stream::wrappers::BroadcastStream;
use zksync_os_types::{
    L1UpgradeEnvelope, ProtocolSemanticVersion, UpgradeTransaction, ZkTransaction,
};

#[derive(Clone)]
pub struct UpgradeSubpool {
    inner: Arc<RwLock<Inner>>,
}

struct Inner {
    current_protocol_version: ProtocolSemanticVersion,
    notify: Arc<Notify>,
    sender: broadcast::Sender<UpgradeTransaction>,
    pending_txs: VecDeque<UpgradeTransaction>,
}

impl UpgradeSubpool {
    pub fn new(current_protocol_version: ProtocolSemanticVersion) -> Self {
        Self {
            inner: Arc::new(RwLock::new(Inner {
                current_protocol_version,
                notify: Arc::new(Notify::new()),
                sender: broadcast::Sender::new(1),
                pending_txs: VecDeque::new(),
            })),
        }
    }

    pub fn upgrade_info_stream(&self) -> UpgradeInfoStream {
        let inner = self.inner.read().unwrap();
        let state = if let Some(pending_tx) = inner.pending_txs.back() {
            StreamState::Pending(pending_tx.clone())
        } else {
            StreamState::Empty
        };
        UpgradeInfoStream {
            receiver: BroadcastStream::new(inner.sender.subscribe()),
            state,
        }
    }

    pub fn insert(&self, tx: UpgradeTransaction) {
        let mut inner = self.inner.write().unwrap();
        let _ = inner.sender.send(tx.clone());
        inner.pending_txs.push_front(tx);
        inner.notify.notify_waiters();
    }

    async fn pop_wait(&self) -> UpgradeTransaction {
        loop {
            let notify = {
                let mut inner = self.inner.write().unwrap();
                if let Some(pending_tx) = inner.pending_txs.pop_back() {
                    tracing::info!(protocol_version = %pending_tx.protocol_version, "advancing protocol version");
                    // Update current protocol version as if the upgrade got applied
                    inner.current_protocol_version = pending_tx.protocol_version.clone();
                    return pending_tx;
                } else {
                    inner.notify.clone()
                }
            };
            notify.notified().await;
        }
    }

    pub async fn on_canonical_state_change(
        &self,
        protocol_version: &ProtocolSemanticVersion,
        txs: Vec<&L1UpgradeEnvelope>,
    ) {
        if txs.is_empty() {
            return;
        }

        for tx in txs {
            // Skip fetched patch upgrades
            let pending_tx = loop {
                let pending_upgrade_info = self.pop_wait().await;
                if let Some(pending_tx) = pending_upgrade_info.tx {
                    break pending_tx;
                }
            };
            assert_eq!(tx, &pending_tx);
        }

        loop {
            let current_protocol_version =
                self.inner.read().unwrap().current_protocol_version.clone();
            if &current_protocol_version == protocol_version {
                return;
            } else if &current_protocol_version < protocol_version {
                let pending_tx = self.pop_wait().await;
                if pending_tx.tx.is_some() {
                    panic!(
                        "expected patch protocol upgrade {}->{} but found minor protocol upgrade {} with unapplied upgrade transaction",
                        current_protocol_version, protocol_version, pending_tx.protocol_version
                    );
                }
            } else {
                panic!(
                    "current protocol version ({current_protocol_version}) is larger than block's protocol version ({protocol_version})",
                );
            }
        }
    }
}

pub struct UpgradeInfoStream {
    receiver: BroadcastStream<UpgradeTransaction>,
    state: StreamState,
}

enum StreamState {
    Empty,
    Pending(UpgradeTransaction),
    Closed,
}

impl Stream for UpgradeInfoStream {
    type Item = UpgradeTransaction;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.as_mut();
        match &this.state {
            StreamState::Empty => {}
            StreamState::Pending(tx) => {
                let tx = tx.clone();
                this.state = StreamState::Closed;
                return Poll::Ready(Some(tx));
            }
            StreamState::Closed => {
                return Poll::Ready(None);
            }
        }

        match this.receiver.poll_next_unpin(cx) {
            Poll::Ready(Some(Ok(tx))) => {
                this.state = StreamState::Closed;
                Poll::Ready(Some(tx))
            }
            Poll::Pending => Poll::Pending,
            Poll::Ready(_) => Poll::Ready(None),
        }
    }
}

pub struct UpgradeTransactionsStream {
    tx: Option<L1UpgradeEnvelope>,
}

impl UpgradeTransactionsStream {
    pub fn empty() -> Self {
        UpgradeTransactionsStream { tx: None }
    }

    pub fn one(tx: L1UpgradeEnvelope) -> Self {
        UpgradeTransactionsStream { tx: Some(tx) }
    }
}

impl Stream for UpgradeTransactionsStream {
    type Item = ZkTransaction;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if let Some(tx) = this.tx.take() {
            Poll::Ready(Some(tx.into()))
        } else {
            Poll::Ready(None)
        }
    }
}

impl TxStream for UpgradeTransactionsStream {
    fn mark_last_tx_as_invalid(self: Pin<&mut Self>) {
        panic!("cannot mark upgrade transaction as invalid")
    }
}
