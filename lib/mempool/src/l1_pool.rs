use alloy::consensus::Transaction;
use dashmap::DashMap;
use std::fmt::Debug;
use std::sync::Arc;
use zksync_os_types::L1Envelope;

// todo: not sure we need this separate from L1Watcher -
// todo: perhaps we should just add a `transactions: DashMap<u64, L1Transaction>` to L1Watcher?

#[auto_impl::auto_impl(&, Box, Arc)]
pub trait L1Pool: Send + Sync + Debug + 'static {
    /// Alternative for [`Clone::clone`] that is object safe.
    fn dyn_clone(&self) -> Box<dyn L1Pool>;

    fn add_transaction(&self, transaction: L1Envelope);

    fn get(&self, id: u64) -> Option<L1Envelope>;
}

impl Clone for Box<dyn L1Pool> {
    fn clone(&self) -> Self {
        self.dyn_clone()
    }
}

/// Thread-safe FIFO mempool that can be polled as a `Stream`.
///
/// Doesn't respect nonces
#[derive(Clone, Debug)]
pub struct L1Mempool {
    transactions: Arc<DashMap<u64, L1Envelope>>,
}

impl L1Mempool {
    pub fn new() -> Self {
        let transactions = DashMap::new();
        Self {
            transactions: Arc::new(transactions),
        }
    }
}

impl L1Pool for L1Mempool {
    fn dyn_clone(&self) -> Box<dyn L1Pool> {
        Box::new(self.clone())
    }

    fn add_transaction(&self, transaction: L1Envelope) {
        self.transactions.insert(transaction.nonce(), transaction);
    }

    fn get(&self, id: u64) -> Option<L1Envelope> {
        self.transactions.get(&id).map(|t| t.clone())
    }
}
