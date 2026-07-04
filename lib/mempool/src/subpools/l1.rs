use futures::{Stream, StreamExt};
use std::collections::{BTreeMap, VecDeque};
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
    /// Every priority transaction seen from L1 that could still appear in a block that
    /// is not committed yet, keyed by serial id. Unlike `pending_txs`, entries survive
    /// block *inclusion* and are dropped only once the committed chain moves past them:
    /// consensus verification authenticates leader-proposed L1 transactions against
    /// this map, and proposals run ahead of commits.
    seen_txs: BTreeMap<L1TxSerialId, Arc<L1PriorityEnvelope>>,
    /// The highest serial id ever inserted (monotonic, never pruned). Lets a verifier
    /// distinguish "my L1 watcher has not seen this id yet" from "I have diverging
    /// data" for ids at the frontier.
    seen_watermark: Option<L1TxSerialId>,
}

impl L1Subpool {
    pub fn new(channel_size: usize) -> Self {
        Self {
            notify: Arc::new(Notify::new()),
            inner: Arc::new(RwLock::new(Inner {
                sender: None,
                pending_txs: VecDeque::new(),
                seen_txs: BTreeMap::new(),
                seen_watermark: None,
            })),
            channel_size,
        }
    }

    /// The locally-watched priority transaction with this serial id (if seen and not
    /// yet passed by the committed chain), plus the highest id seen so far.
    pub async fn seen_priority_tx(
        &self,
        id: L1TxSerialId,
    ) -> (Option<Arc<L1PriorityEnvelope>>, Option<L1TxSerialId>) {
        let inner = self.inner.read().await;
        (inner.seen_txs.get(&id).cloned(), inner.seen_watermark)
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
        let id = tx.priority_id();
        inner.seen_txs.insert(id, tx.clone());
        inner.seen_watermark = Some(inner.seen_watermark.map_or(id, |mark| mark.max(id)));
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

        // The committed chain has moved past these ids; no future valid block can
        // include them again (ids ratchet), so their authenticity records can go.
        let mut inner = self.inner.write().await;
        inner.seen_txs = inner.seen_txs.split_off(&(priority_id + 1));

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

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{Address, B256, Bytes, U256};
    use std::marker::PhantomData;
    use zksync_os_types::{L1PriorityTxType, L1Tx};

    fn priority_tx(id: L1TxSerialId) -> Arc<L1PriorityEnvelope> {
        Arc::new(L1PriorityEnvelope {
            inner: L1Tx::<L1PriorityTxType> {
                hash: B256::repeat_byte(id as u8 + 1),
                initiator: Address::ZERO,
                to: Address::ZERO,
                gas_limit: 0,
                gas_per_pubdata_byte_limit: 0,
                max_fee_per_gas: 0,
                max_priority_fee_per_gas: 0,
                nonce: id,
                value: U256::ZERO,
                to_mint: U256::ZERO,
                refund_recipient: Address::ZERO,
                input: Bytes::new(),
                factory_deps: Vec::new(),
                marker: PhantomData,
            },
        })
    }

    #[tokio::test]
    async fn seen_txs_survive_inclusion_and_prune_at_commit() {
        let mut subpool = L1Subpool::new(4);
        let txs: Vec<_> = (0..3).map(priority_tx).collect();
        for tx in &txs {
            subpool.insert(tx.clone()).await;
        }

        // All watched transactions are known, and the watermark tracks the frontier.
        let (found, watermark) = subpool.seen_priority_tx(1).await;
        assert_eq!(found.as_deref(), Some(txs[1].as_ref()));
        assert_eq!(watermark, Some(2));

        // Committing a block with the first two prunes exactly those: the chain can
        // never include them again, while id 2 may still appear in a proposal.
        subpool
            .on_canonical_state_change(vec![txs[0].as_ref(), txs[1].as_ref()])
            .await;
        assert!(subpool.seen_priority_tx(0).await.0.is_none());
        assert!(subpool.seen_priority_tx(1).await.0.is_none());
        assert_eq!(
            subpool.seen_priority_tx(2).await.0.as_deref(),
            Some(txs[2].as_ref())
        );
        // The watermark is monotonic — pruning does not roll it back.
        assert_eq!(subpool.seen_priority_tx(2).await.1, Some(2));
    }
}
