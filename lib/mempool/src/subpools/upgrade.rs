use futures::{Stream, StreamExt};
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::task::{Context, Poll, ready};
use tokio::sync::{Notify, broadcast};
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
use zksync_os_types::{L1UpgradeEnvelope, ProtocolSemanticVersion, UpgradeInfo, ZkTransaction};

#[derive(Clone)]
pub struct UpgradeSubpool {
    notify: Arc<Notify>,
    inner: Arc<RwLock<Inner>>,
}

struct Inner {
    /// Tracks currently active protocol version. Needed because of patch upgrades that do not come
    /// with an upgrade transaction.
    current_protocol_version: ProtocolSemanticVersion,
    sender: broadcast::Sender<UpgradeInfo>,
    pending_upgrades: VecDeque<UpgradeInfo>,
}

impl UpgradeSubpool {
    pub fn new(current_protocol_version: ProtocolSemanticVersion) -> Self {
        Self {
            notify: Arc::new(Notify::new()),
            inner: Arc::new(RwLock::new(Inner {
                current_protocol_version,
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
            StreamState::Empty(BroadcastStream::new(inner.sender.subscribe()))
        };
        UpgradeInfoStream { state }
    }

    pub fn insert(&self, upgrade: UpgradeInfo) {
        let mut inner = self.inner.write().unwrap();
        let _ = inner.sender.send(upgrade.clone());
        inner.pending_upgrades.push_front(upgrade);
        self.notify.notify_waiters();
    }

    async fn pop_wait(&self) -> UpgradeInfo {
        loop {
            let notified = self.notify.notified();
            {
                let mut inner = self.inner.write().unwrap();
                if let Some(upgrade) = inner.pending_upgrades.pop_back() {
                    tracing::info!(protocol_version = %upgrade.protocol_version(), "advancing protocol version");
                    return upgrade;
                }
            }
            notified.await;
        }
    }

    pub async fn on_canonical_state_change(
        &self,
        protocol_version: &ProtocolSemanticVersion,
        txs: Vec<&L1UpgradeEnvelope>,
    ) {
        // We track current protocol version that we end up with after applying upgrade transaction.
        let mut current_protocol_version =
            self.inner.read().unwrap().current_protocol_version.clone();

        // If there are no upgrade transactions and current protocol version matches the one in the
        // block, we do not have to do anything.
        if txs.is_empty() && protocol_version == &current_protocol_version {
            return;
        }

        for tx in txs {
            // Skip fetched patch upgrades
            let pending_tx = loop {
                let pending_upgrade_info = self.pop_wait().await;
                // Update current protocol version with discovered upgrade.
                current_protocol_version = pending_upgrade_info.protocol_version().clone();
                if let Some(pending_tx) = pending_upgrade_info.tx {
                    break pending_tx;
                }
            };
            assert_eq!(tx, &pending_tx);
        }

        // We need to make sure that our current protocol version matches the one in the block.
        // For patch upgrades there might be no upgrade transaction, but we still have to find and
        // consume relevant upgrade info from L1 watcher.
        loop {
            if &current_protocol_version == protocol_version {
                break;
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
                // Update current protocol version with discovered upgrade and do one more iteration.
                current_protocol_version = upgrade.protocol_version().clone();
            } else {
                panic!(
                    "current protocol version ({current_protocol_version}) is larger than block's protocol version ({protocol_version})",
                );
            }
        }

        // Write protocol version we ended up with.
        self.inner.write().unwrap().current_protocol_version = current_protocol_version;
    }
}

pub struct UpgradeInfoStream {
    state: StreamState,
}

/// State machine to ensure we serve up to one upgrade transaction.
#[allow(clippy::large_enum_variant)]
enum StreamState {
    /// No discovered upgrade yet, streaming from L1 watcher subscription.
    Empty(BroadcastStream<UpgradeInfo>),
    /// Upgrade has been previously discovered.
    Pending(UpgradeInfo),
    /// Stream is closed because either one upgrade was already returned or upstream receiver was
    /// closed prematurely.
    Closed,
}

impl Stream for UpgradeInfoStream {
    type Item = UpgradeInfo;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.as_mut();
        match &mut this.state {
            StreamState::Empty(receiver) => {
                let Some(result) = ready!(receiver.poll_next_unpin(cx)) else {
                    tracing::debug!("upgrade watcher stream is closed");
                    this.state = StreamState::Closed;
                    return Poll::Ready(None);
                };
                match result {
                    Ok(upgrade) => {
                        this.state = StreamState::Closed;
                        Poll::Ready(Some(upgrade))
                    }
                    Err(BroadcastStreamRecvError::Lagged(count)) => {
                        // Fatal error as we lost at least one upgrade
                        panic!("upgrade receiver lagged by {count} items");
                    }
                }
            }
            StreamState::Pending(upgrade) => {
                let upgrade = upgrade.clone();
                this.state = StreamState::Closed;
                Poll::Ready(Some(upgrade))
            }
            StreamState::Closed => Poll::Ready(None),
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
