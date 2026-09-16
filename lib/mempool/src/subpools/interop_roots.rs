use futures::stream::BoxStream;
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, RwLock};
use tokio::sync::Notify;
use tokio::time::Instant;
use tokio::time::sleep_until;
use zksync_os_types::{
    IndexedInteropRoot, InteropRoot, SystemTxEnvelope, SystemTxType, ZkTransaction,
};

#[derive(Clone)]
pub struct InteropRootsSubpool {
    /// Consistent state of pending roots shared between all clones of this subpool.
    inner: Arc<RwLock<Inner>>,
    notify: Arc<Notify>,
    interop_roots_per_tx: usize,
}

/// Holds all **pending** interop roots, i.e. those that have been received but not included in the
/// canonical chain yet. Note that some prefix might have already been executed in sequencer (as
/// they were returned from [`InteropRootsSubpool::interop_transactions_with_delay`]).
struct Inner {
    pending_roots: BTreeMap<u64, InteropRoot>,
}

impl InteropRootsSubpool {
    pub fn new(interop_roots_per_tx: usize) -> Self {
        Self {
            inner: Arc::new(RwLock::new(Inner {
                pending_roots: BTreeMap::new(),
            })),
            notify: Arc::new(Notify::new()),
            interop_roots_per_tx,
        }
    }

    pub async fn interop_transactions_with_delay(
        &self,
        next_tx_allowed_after: Instant,
    ) -> BoxStream<'_, ZkTransaction> {
        Box::pin(futures::stream::unfold(
            (
                self.inner.clone(),
                self.notify.clone(),
                0u64,
                VecDeque::<(u64, InteropRoot)>::default(),
            ),
            move |(inner, notify, mut cursor, mut buffer)| async move {
                sleep_until(next_tx_allowed_after).await;
                loop {
                    // Subscribe BEFORE reading — avoids the race where an insert
                    // happens between our read and our .notified().await.
                    let notified = notify.notified();

                    {
                        let inner = inner.read().unwrap();
                        for (id, root) in inner.pending_roots.range(cursor..) {
                            cursor = id + 1;
                            buffer.push_front((*id, root.clone()));
                        }
                    }

                    if !buffer.is_empty() {
                        let amount_of_roots_to_take = buffer.len().min(self.interop_roots_per_tx);
                        let starting_index = buffer.len() - amount_of_roots_to_take;

                        let roots_to_consume: Vec<(u64, InteropRoot)> = buffer
                            .drain(starting_index..)
                            .rev() // reversing iterator as last element is the one received earliest
                            .collect();

                        // Use the log_id of the last (largest) root as the salt for uniqueness.
                        let last_log_id = roots_to_consume
                            .last()
                            .expect("roots_to_consume is non-empty")
                            .0;
                        let roots = roots_to_consume.into_iter().map(|(_, r)| r).collect();
                        let envelope = SystemTxEnvelope::import_interop_roots(roots, last_log_id);
                        drop(notified);
                        return Some((envelope.into(), (inner, notify, cursor, buffer)));
                    }

                    // Nothing new yet — wait for an insert, then retry.
                    notified.await;
                }
            },
        ))
    }

    pub async fn add_root(&mut self, root: IndexedInteropRoot) {
        self.inner
            .write()
            .unwrap()
            .pending_roots
            .insert(root.log_id, root.root);
        self.notify.notify_waiters();
    }

    async fn pop_wait(&self) -> (u64, InteropRoot) {
        loop {
            let notified = self.notify.notified();
            {
                let mut inner = self.inner.write().unwrap();
                if let Some((id, root)) = inner.pending_roots.pop_first() {
                    return (id, root);
                }
            }
            notified.await;
        }
    }

    /// Cleans up the stream and removes all roots that were sent in transactions.
    /// Returns the last log_id of the executed interop root.
    ///
    /// A produced block (`strict_subpool_cleanup`) was built from this very subpool, so its roots
    /// are consumed from it and checked against the transaction. Replay and rebuild instead take
    /// the transaction as the source of truth and drop every pending root up to the one it
    /// imported: a node must not have to wait for its own L1 watcher to have observed those roots.
    /// The watcher only starts scanning L1 once the first block is replayed, and a node that
    /// restricts import (`sequencer.interop_root_source_chains`) never observes some of them at
    /// all — waiting would stall replay behind an L1 scan, or forever.
    pub async fn on_canonical_state_change(
        &self,
        txs: Vec<&SystemTxEnvelope>,
        strict_subpool_cleanup: bool,
    ) -> Option<u64> {
        if txs.is_empty() {
            return None;
        }

        if !strict_subpool_cleanup {
            let last_log_id = txs
                .into_iter()
                .filter(|tx| matches!(tx.system_subtype(), SystemTxType::ImportInteropRoots(_)))
                .map(|tx| tx.interop_roots_last_log_id())
                .max()?;
            let mut inner = self.inner.write().unwrap();
            let remaining = inner
                .pending_roots
                .split_off(&last_log_id.saturating_add(1));
            inner.pending_roots = remaining;
            return Some(last_log_id);
        }

        let mut last_log_id = None;

        for tx in txs {
            let SystemTxType::ImportInteropRoots(roots_count) = *tx.system_subtype() else {
                continue;
            };

            let mut roots = Vec::with_capacity(roots_count as usize);
            let mut tx_last_log_id = None;
            for _ in 0..roots_count {
                let (id, root) = self.pop_wait().await;
                roots.push(root);
                tx_last_log_id = Some(id);
            }
            last_log_id = tx_last_log_id;
            let envelope = SystemTxEnvelope::import_interop_roots(
                roots,
                tx_last_log_id.expect("roots_count > 0"),
            );

            assert_eq!(&envelope, tx);
        }

        last_log_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{B256, U256};

    fn root(chain_id: u64) -> InteropRoot {
        InteropRoot {
            chainId: U256::from(chain_id),
            blockOrBatchNumber: U256::from(1),
            sides: vec![B256::ZERO],
        }
    }

    fn import_tx(log_ids: impl IntoIterator<Item = u64>) -> SystemTxEnvelope {
        let log_ids: Vec<_> = log_ids.into_iter().collect();
        let last_log_id = *log_ids.last().unwrap();
        SystemTxEnvelope::import_interop_roots(
            log_ids.into_iter().map(|_| root(1)).collect(),
            last_log_id,
        )
    }

    async fn subpool_with(log_ids: impl IntoIterator<Item = u64>) -> InteropRootsSubpool {
        let mut subpool = InteropRootsSubpool::new(50);
        for log_id in log_ids {
            subpool
                .add_root(IndexedInteropRoot {
                    log_id,
                    root: root(1),
                })
                .await;
        }
        subpool
    }

    fn pending_ids(subpool: &InteropRootsSubpool) -> Vec<u64> {
        subpool
            .inner
            .read()
            .unwrap()
            .pending_roots
            .keys()
            .copied()
            .collect()
    }

    #[tokio::test]
    async fn strict_cleanup_consumes_executed_roots() {
        let subpool = subpool_with([0, 1, 2]).await;

        let tx = import_tx([0, 1]);
        let last_log_id = subpool.on_canonical_state_change(vec![&tx], true).await;

        assert_eq!(last_log_id, Some(1));
        assert_eq!(pending_ids(&subpool), vec![2]);
    }

    /// Replay must not wait for roots this node never watched — the transaction alone says how far
    /// the interop cursor moved.
    #[tokio::test]
    async fn replay_cleanup_does_not_need_the_roots_locally() {
        let subpool = subpool_with([]).await;

        let tx = import_tx([7, 8]);
        let last_log_id = subpool.on_canonical_state_change(vec![&tx], false).await;

        assert_eq!(last_log_id, Some(8));
        assert!(pending_ids(&subpool).is_empty());
    }

    #[tokio::test]
    async fn replay_cleanup_drops_every_root_up_to_the_imported_one() {
        let subpool = subpool_with([3, 4, 5, 9]).await;

        let tx = import_tx([5]);
        let last_log_id = subpool.on_canonical_state_change(vec![&tx], false).await;

        assert_eq!(last_log_id, Some(5));
        assert_eq!(pending_ids(&subpool), vec![9]);
    }

    #[tokio::test]
    async fn replay_cleanup_reports_the_last_import_of_the_block() {
        let subpool = subpool_with([0, 1, 2, 3]).await;

        let first = import_tx([0, 1]);
        let second = import_tx([2]);
        let last_log_id = subpool
            .on_canonical_state_change(vec![&first, &second], false)
            .await;

        assert_eq!(last_log_id, Some(2));
        assert_eq!(pending_ids(&subpool), vec![3]);
    }

    #[tokio::test]
    async fn cleanup_without_import_transactions_is_a_no_op() {
        let subpool = subpool_with([0]).await;

        assert_eq!(subpool.on_canonical_state_change(vec![], false).await, None);
        assert_eq!(pending_ids(&subpool), vec![0]);
    }
}
