use crate::TxStream;
use futures::{Stream, StreamExt};
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::task::{Context, Poll};
use tokio::sync::{Notify, broadcast};
use tokio_stream::wrappers::BroadcastStream;
use zksync_os_types::{L1UpgradeEnvelope, ProtocolSemanticVersion, UpgradeInfo, ZkTransaction};

#[derive(Clone)]
pub struct UpgradeSubpool {
    inner: Arc<RwLock<Inner>>,
}

struct Inner {
    current_protocol_version: ProtocolSemanticVersion,
    notify: Arc<Notify>,
    sender: broadcast::Sender<UpgradeInfo>,
    pending_upgrades: VecDeque<UpgradeInfo>,
}

impl UpgradeSubpool {
    pub fn new(current_protocol_version: ProtocolSemanticVersion) -> Self {
        Self {
            inner: Arc::new(RwLock::new(Inner {
                current_protocol_version,
                notify: Arc::new(Notify::new()),
                sender: broadcast::Sender::new(1),
                pending_upgrades: VecDeque::new(),
            })),
        }
    }

    pub fn upgrade_info_stream(&self) -> UpgradeInfoStream {
        let inner = self.inner.read().unwrap();
        let state = if let Some(pending_tx) = inner.pending_upgrades.back() {
            StreamState::Pending(pending_tx.clone())
        } else {
            StreamState::Empty
        };
        UpgradeInfoStream {
            receiver: BroadcastStream::new(inner.sender.subscribe()),
            state,
        }
    }

    pub fn insert(&self, upgrade: UpgradeInfo) {
        let mut inner = self.inner.write().unwrap();
        let _ = inner.sender.send(upgrade.clone());
        inner.pending_upgrades.push_front(upgrade);
        inner.notify.notify_waiters();
    }

    async fn pop_wait(&self) -> UpgradeInfo {
        loop {
            let notify = {
                let mut inner = self.inner.write().unwrap();
                if let Some(upgrade) = inner.pending_upgrades.pop_back() {
                    tracing::info!(protocol_version = %upgrade.protocol_version(), "advancing protocol version");
                    // Update current protocol version as if the upgrade got applied
                    inner.current_protocol_version = upgrade.protocol_version().clone();
                    return upgrade;
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
                let upgrade = self.pop_wait().await;
                if upgrade.tx.is_some() {
                    panic!(
                        "expected patch protocol upgrade {}->{} but found minor protocol upgrade {} with unapplied upgrade transaction",
                        current_protocol_version,
                        protocol_version,
                        upgrade.protocol_version()
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
    receiver: BroadcastStream<UpgradeInfo>,
    state: StreamState,
}

#[allow(clippy::large_enum_variant)]
enum StreamState {
    Empty,
    Pending(UpgradeInfo),
    Closed,
}

impl Stream for UpgradeInfoStream {
    type Item = UpgradeInfo;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.as_mut();
        match &this.state {
            StreamState::Empty => {}
            StreamState::Pending(upgrade) => {
                let upgrade = upgrade.clone();
                this.state = StreamState::Closed;
                return Poll::Ready(Some(upgrade));
            }
            StreamState::Closed => {
                return Poll::Ready(None);
            }
        }

        match this.receiver.poll_next_unpin(cx) {
            Poll::Ready(Some(Ok(upgrade))) => {
                this.state = StreamState::Closed;
                Poll::Ready(Some(upgrade))
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
