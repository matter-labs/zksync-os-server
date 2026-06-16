use futures::{Stream, StreamExt};
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, ready};
use tokio::sync::{Notify, RwLock, mpsc};
use tokio_stream::wrappers::ReceiverStream;
use zksync_os_types::{L1UpgradeEnvelope, ProtocolSemanticVersion, UpgradeInfo, ZkTransaction};

/// New upgrades are added to `Inner` as well as it's used to create `UpgradeInfoStream`.
/// `sender` is used to submit new upgrades to the active stream.
/// If there is no active stream, then sender will be dropped on the next access; tx is inserted to `pending_txs` anyway.
#[derive(Clone)]
pub struct UpgradeSubpool {
    notify: Arc<Notify>,
    inner: Arc<RwLock<Inner>>,
}

struct Inner {
    /// Tracks currently active protocol version. Needed because of patch upgrades that do not come
    /// with an upgrade transaction.
    current_protocol_version: Option<ProtocolSemanticVersion>,
    sender: Option<mpsc::Sender<UpgradeInfo>>,
    pending_upgrades: VecDeque<UpgradeInfo>,
}

impl Default for UpgradeSubpool {
    fn default() -> Self {
        Self {
            notify: Arc::new(Notify::new()),
            inner: Arc::new(RwLock::new(Inner {
                current_protocol_version: None,
                sender: None,
                pending_upgrades: VecDeque::new(),
            })),
        }
    }
}

impl UpgradeSubpool {
    pub(crate) async fn init(&self, current_protocol_version: ProtocolSemanticVersion) {
        let mut inner = self.inner.write().await;
        assert!(
            inner.current_protocol_version.is_none(),
            "tried to re-initialize UpgradeSubpool"
        );
        inner.current_protocol_version = Some(current_protocol_version);
    }

    pub async fn upgrade_info_stream(&self) -> UpgradeInfoStream {
        // `1` as buffer is enough because upgrade transactions are rare.
        let (sender, receiver) = mpsc::channel(1);
        let mut inner = self.inner.write().await;
        inner.sender = Some(sender);
        let state = if let Some(pending_tx) = inner.pending_upgrades.back() {
            StreamState::Pending(pending_tx.clone())
        } else {
            StreamState::Empty(ReceiverStream::new(receiver))
        };
        UpgradeInfoStream { state }
    }

    pub async fn insert(&self, upgrade: UpgradeInfo) {
        let mut inner = self.inner.write().await;
        if let Some(sender) = &inner.sender {
            // If the receiver has been dropped, we should stop sending transactions and clear the sender to avoid unnecessary work.
            if sender.send(upgrade.clone()).await.is_err() {
                inner.sender.take();
            }
        }
        inner.pending_upgrades.push_front(upgrade);
        self.notify.notify_waiters();
    }

    async fn pop_wait(&self) -> UpgradeInfo {
        loop {
            let notified = self.notify.notified();
            {
                let mut inner = self.inner.write().await;
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
        let mut current_protocol_version = self
            .inner
            .read()
            .await
            .current_protocol_version
            .clone()
            .expect("uninitialized subpool");

        if txs.is_empty() && protocol_version == &current_protocol_version {
            return;
        }

        // Older upgrade logs cannot be found or processed reliably by the current
        // watcher. During replay, the ReplayRecord is the source of truth.
        self.inner.write().await.pending_upgrades.retain(|upgrade| {
            upgrade.protocol_version()
                >= &ProtocolSemanticVersion::MIN_VERSION_WITH_RELIABLE_UPGRADE_LOGS
        });
        if protocol_version < &ProtocolSemanticVersion::MIN_VERSION_WITH_RELIABLE_UPGRADE_LOGS {
            current_protocol_version = protocol_version.clone();
        } else {
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
        }
        // Write protocol version we ended up with.
        self.inner.write().await.current_protocol_version = Some(current_protocol_version);
    }
}

pub struct UpgradeInfoStream {
    state: StreamState,
}

/// State machine to ensure we serve up to one upgrade transaction.
#[allow(clippy::large_enum_variant)]
enum StreamState {
    /// No discovered upgrade yet, streaming from L1 watcher subscription.
    Empty(ReceiverStream<UpgradeInfo>),
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
                let Some(upgrade) = ready!(receiver.poll_next_unpin(cx)) else {
                    tracing::debug!("upgrade watcher stream is closed");
                    this.state = StreamState::Closed;
                    return Poll::Ready(None);
                };
                this.state = StreamState::Closed;
                Poll::Ready(Some(upgrade))
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

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{Address, B256, Bytes, U256};
    use std::marker::PhantomData;
    use std::time::Duration;
    use zksync_os_types::{L1Tx, UpgradeMetadata, UpgradeTxType};

    fn version(minor: u64, patch: u64) -> ProtocolSemanticVersion {
        ProtocolSemanticVersion::new(0, minor, patch)
    }

    fn upgrade_tx(hash_byte: u8) -> L1UpgradeEnvelope {
        L1UpgradeEnvelope {
            inner: L1Tx::<UpgradeTxType> {
                hash: B256::repeat_byte(hash_byte),
                initiator: Address::ZERO,
                to: Address::ZERO,
                gas_limit: 0,
                gas_per_pubdata_byte_limit: 0,
                max_fee_per_gas: 0,
                max_priority_fee_per_gas: 0,
                nonce: 0,
                value: U256::ZERO,
                to_mint: U256::ZERO,
                refund_recipient: Address::ZERO,
                input: Bytes::new(),
                factory_deps: Vec::new(),
                marker: PhantomData,
            },
        }
    }

    fn upgrade_info(
        protocol_version: ProtocolSemanticVersion,
        tx: Option<L1UpgradeEnvelope>,
    ) -> UpgradeInfo {
        UpgradeInfo {
            tx,
            metadata: UpgradeMetadata {
                timestamp: 0,
                protocol_version,
                force_preimages: Vec::new(),
            },
        }
    }

    #[tokio::test]
    async fn historical_upgrade_replay_does_not_wait_for_watcher() {
        let unreliable_version = version(30, 1);
        let subpool = UpgradeSubpool::default();
        subpool
            .init(ProtocolSemanticVersion::legacy_genesis_version())
            .await;
        let tx = upgrade_tx(1);

        tokio::time::timeout(
            Duration::from_millis(50),
            subpool.on_canonical_state_change(&unreliable_version, vec![&tx]),
        )
        .await
        .expect("historical upgrade replay should not wait for watcher data");

        assert_eq!(
            subpool.inner.read().await.current_protocol_version,
            Some(unreliable_version)
        );
    }

    #[tokio::test]
    async fn historical_replay_drains_pending_old_upgrades() {
        let subpool = UpgradeSubpool::default();
        subpool
            .init(ProtocolSemanticVersion::legacy_genesis_version())
            .await;
        let old_tx = upgrade_tx(1);
        let new_tx = upgrade_tx(2);
        let new_version = version(31, 0);
        let unreliable_version = version(30, 1);
        subpool
            .insert(upgrade_info(unreliable_version.clone(), Some(old_tx)))
            .await;
        subpool
            .insert(upgrade_info(new_version.clone(), Some(new_tx.clone())))
            .await;

        subpool
            .on_canonical_state_change(&unreliable_version, Vec::new())
            .await;

        let mut stream = subpool.upgrade_info_stream().await;
        let remaining = stream
            .next()
            .await
            .expect("newer pending upgrade should not be drained");
        assert_eq!(remaining.protocol_version(), &new_version);
        assert_eq!(remaining.tx.as_ref(), Some(&new_tx));
    }

    #[tokio::test]
    async fn supported_upgrade_waits_for_watcher_and_validates_tx() {
        let subpool = UpgradeSubpool::default();
        subpool
            .init(ProtocolSemanticVersion::MIN_VERSION_WITH_RELIABLE_UPGRADE_LOGS)
            .await;
        let tx = upgrade_tx(1);
        let target_version = version(31, 0);

        assert!(
            tokio::time::timeout(
                Duration::from_millis(50),
                subpool.on_canonical_state_change(&target_version, vec![&tx]),
            )
            .await
            .is_err(),
            "supported upgrade should wait for watcher data"
        );

        subpool
            .insert(upgrade_info(target_version.clone(), Some(tx.clone())))
            .await;
        tokio::time::timeout(
            Duration::from_millis(50),
            subpool.on_canonical_state_change(&target_version, vec![&tx]),
        )
        .await
        .expect("matching watcher upgrade should validate");

        assert_eq!(
            subpool.inner.read().await.current_protocol_version,
            Some(target_version)
        );
    }

    #[tokio::test]
    async fn supported_patch_upgrade_consumes_watcher_metadata() {
        let subpool = UpgradeSubpool::default();
        subpool
            .init(ProtocolSemanticVersion::MIN_VERSION_WITH_RELIABLE_UPGRADE_LOGS)
            .await;
        let target_version = version(30, 3);
        subpool
            .insert(upgrade_info(target_version.clone(), None))
            .await;

        tokio::time::timeout(
            Duration::from_millis(50),
            subpool.on_canonical_state_change(&target_version, Vec::new()),
        )
        .await
        .expect("patch upgrade metadata should be consumed");

        assert_eq!(
            subpool.inner.read().await.current_protocol_version,
            Some(target_version)
        );
    }
}
