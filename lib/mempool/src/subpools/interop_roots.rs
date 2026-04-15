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
    pub async fn on_canonical_state_change(&self, txs: Vec<&SystemTxEnvelope>) -> Option<u64> {
        if txs.is_empty() {
            return None;
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
