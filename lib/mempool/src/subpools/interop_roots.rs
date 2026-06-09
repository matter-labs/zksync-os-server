use alloy::primitives::B256;
use futures::stream::BoxStream;
use std::collections::{BTreeMap, HashMap, VecDeque};
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
    /// Per-emitted-envelope manifest: which log_ids went into each envelope the
    /// stream has produced and is still waiting to see canonicalized. Keyed by
    /// the envelope hash. `on_canonical_state_change` looks the canonical tx up
    /// here and removes exactly those log_ids from `pending_roots`, so cleanup
    /// does not depend on canonical tx callback ordering.
    emitted_manifest: HashMap<B256, Vec<u64>>,
}

impl InteropRootsSubpool {
    pub fn new(interop_roots_per_tx: usize) -> Self {
        assert!(
            interop_roots_per_tx > 0,
            "interop_roots_per_tx must be greater than zero"
        );
        Self {
            inner: Arc::new(RwLock::new(Inner {
                pending_roots: BTreeMap::new(),
                emitted_manifest: HashMap::new(),
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
                        let log_ids: Vec<u64> =
                            roots_to_consume.iter().map(|(id, _)| *id).collect();
                        let roots: Vec<InteropRoot> =
                            roots_to_consume.into_iter().map(|(_, r)| r).collect();
                        let envelope = SystemTxEnvelope::import_interop_roots(roots, last_log_id);
                        // Record which log_ids this envelope drew from so the
                        // canonicalization callback can remove exactly those
                        // entries even if canonical txs are reported in a
                        // different order than the stream yielded them.
                        {
                            let mut guard = inner.write().unwrap();
                            guard.emitted_manifest.insert(*envelope.hash(), log_ids);
                        }
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

    /// Cleans up the stream and removes all roots that were sent in transactions.
    /// Returns the last log_id of the executed interop root.
    pub async fn on_canonical_state_change(&self, txs: Vec<&SystemTxEnvelope>) -> Option<u64> {
        if txs.is_empty() {
            return None;
        }

        let mut last_log_id: Option<u64> = None;

        let mut inner = self.inner.write().unwrap();
        for tx in txs {
            let SystemTxType::ImportInteropRoots(_) = *tx.system_subtype() else {
                continue;
            };

            let Some(log_ids) = inner.emitted_manifest.remove(tx.hash()) else {
                // No manifest entry: the envelope was canonicalized from a
                // path other than this subpool's stream (e.g. replay of a
                // historical block on restart). Best effort: advance the
                // last_log_id watermark from the envelope salt; pending entries
                // (if any) will fall out as the matching log_ids reappear later
                // through `add_root` / canonicalization.
                last_log_id = Some(last_log_id.map_or(tx.salt(), |id| id.max(tx.salt())));
                continue;
            };
            // Drop exactly the log_ids the stream recorded for this envelope.
            // `get` may legitimately return None if a previous canonicalization
            // already removed them.
            for id in &log_ids {
                inner.pending_roots.remove(id);
            }
            if let Some(end_id) = log_ids.last() {
                last_log_id = Some(last_log_id.map_or(*end_id, |id| id.max(*end_id)));
            }
        }

        last_log_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{B256, Uint};
    use futures::StreamExt;
    use zksync_os_types::ZkEnvelope;

    fn root(log_id: u64) -> IndexedInteropRoot {
        IndexedInteropRoot {
            log_id,
            root: InteropRoot {
                chainId: Uint::from(1),
                blockOrBatchNumber: Uint::from(log_id),
                sides: vec![B256::ZERO],
            },
        }
    }

    fn import_roots_count(tx: &ZkTransaction) -> u64 {
        match tx.as_system_tx_type() {
            Some(SystemTxType::ImportInteropRoots(count)) => *count,
            other => panic!("expected import roots tx, got {other:?}"),
        }
    }

    fn system_envelope(tx: &ZkTransaction) -> &SystemTxEnvelope {
        match tx.envelope() {
            ZkEnvelope::System(envelope) => envelope,
            _ => panic!("expected system tx"),
        }
    }

    #[test]
    #[should_panic(expected = "interop_roots_per_tx must be greater than zero")]
    fn rejects_zero_root_limit() {
        let _ = InteropRootsSubpool::new(0);
    }

    #[tokio::test]
    async fn import_transactions_are_chunked_by_configured_limit() {
        let mut subpool = InteropRootsSubpool::new(3);
        for log_id in 1..=8 {
            subpool.add_root(root(log_id)).await;
        }

        let mut stream = subpool
            .interop_transactions_with_delay(Instant::now())
            .await;

        let first = stream.next().await.expect("first import tx");
        let second = stream.next().await.expect("second import tx");
        let third = stream.next().await.expect("third import tx");

        assert_eq!(import_roots_count(&first), 3);
        assert_eq!(import_roots_count(&second), 3);
        assert_eq!(import_roots_count(&third), 2);
    }

    #[tokio::test]
    async fn canonical_cleanup_uses_emitted_manifest() {
        let mut subpool = InteropRootsSubpool::new(2);
        for log_id in 1..=3 {
            subpool.add_root(root(log_id)).await;
        }

        let first = {
            let mut stream = subpool
                .interop_transactions_with_delay(Instant::now())
                .await;
            stream.next().await.expect("first import tx")
        };

        let last_log_id = subpool
            .on_canonical_state_change(vec![system_envelope(&first)])
            .await;
        assert_eq!(last_log_id, Some(2));

        let second = {
            let mut stream = subpool
                .interop_transactions_with_delay(Instant::now())
                .await;
            stream.next().await.expect("second import tx")
        };

        assert_eq!(import_roots_count(&second), 1);
        let last_log_id = subpool
            .on_canonical_state_change(vec![system_envelope(&second)])
            .await;
        assert_eq!(last_log_id, Some(3));
    }
}
