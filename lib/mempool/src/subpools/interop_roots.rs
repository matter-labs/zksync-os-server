use futures::stream::BoxStream;
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, RwLock};
use tokio::sync::Notify;
use tokio::time::Instant;
use tokio::time::sleep_until;
use zksync_os_types::{
    IndexedInteropRoot, InteropRoot, InteropRootsLogIndex, SystemTxEnvelope, SystemTxType,
    ZkTransaction,
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
    pending_roots: BTreeMap<InteropRootsLogIndex, InteropRoot>,
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
    ) -> BoxStream<ZkTransaction> {
        Box::pin(futures::stream::unfold(
            (
                self.inner.clone(),
                self.notify.clone(),
                InteropRootsLogIndex::default(),
                VecDeque::default(),
            ),
            move |(inner, notify, mut cursor, mut buffer)| async move {
                sleep_until(next_tx_allowed_after).await;
                loop {
                    // Subscribe BEFORE reading — avoids the race where an insert
                    // happens between our read and our .notified().await.
                    let notified = notify.notified();

                    {
                        let inner = inner.read().unwrap();
                        for (id, root) in inner.pending_roots.range(&cursor..) {
                            cursor = InteropRootsLogIndex {
                                block_number: id.block_number,
                                index_in_block: id.index_in_block + 1,
                            };
                            buffer.push_front(root.clone());
                        }
                    }

                    if !buffer.is_empty() {
                        let amount_of_roots_to_take = buffer.len().min(self.interop_roots_per_tx);
                        let starting_index = buffer.len() - amount_of_roots_to_take;

                        let roots_to_consume = buffer
                            .drain(starting_index..)
                            .rev() // reversing iterator as last element is the one received earliest
                            .collect::<Vec<_>>();

                        let envelope = SystemTxEnvelope::import_interop_roots(roots_to_consume);
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
            .insert(root.log_index, root.root);
        self.notify.notify_waiters();
    }

    async fn pop_wait(&self) -> (InteropRootsLogIndex, InteropRoot) {
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

    /// Cleans up the stream and removes all roots that were sent in transactions
    /// Returns the last log index of executed interop root
    pub async fn on_canonical_state_change(
        &self,
        txs: Vec<&SystemTxEnvelope>,
    ) -> Option<InteropRootsLogIndex> {
        if txs.is_empty() {
            return None;
        }

        let mut log_index = InteropRootsLogIndex::default();

        for tx in txs {
            let SystemTxType::ImportInteropRoots(roots_count) = *tx.system_subtype() else {
                continue;
            };

            let mut roots = Vec::with_capacity(roots_count as usize);
            for _ in 0..roots_count {
                let (id, root) = self.pop_wait().await;
                roots.push(root);
                log_index = id;
            }
            let envelope = SystemTxEnvelope::import_interop_roots(roots);

            assert_eq!(&envelope, tx);
        }

        Some(log_index)
    }
}
