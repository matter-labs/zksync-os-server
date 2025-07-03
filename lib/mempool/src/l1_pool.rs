use dashmap::DashMap;
use std::fmt::Debug;
use zksync_os_types::L1Transaction;

#[auto_impl::auto_impl(&, Box, Arc)]
pub trait L1Pool: Send + Sync + Debug + 'static {
    /// Alternative for [`Clone::clone`] that is object safe.
    fn dyn_clone(&self) -> Box<dyn L1Pool>;

    fn add_transaction(&self, transaction: L1Transaction);

    fn get(&self, id: u64) -> Option<L1Transaction>;
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
    transactions: DashMap<u64, L1Transaction>,
}

impl L1Mempool {
    pub fn new(forced_transaction: L1Transaction) -> Self {
        let transactions = DashMap::new();
        transactions.insert(forced_transaction.serial_id().0, forced_transaction);
        Self { transactions }
    }
}

impl L1Pool for L1Mempool {
    fn dyn_clone(&self) -> Box<dyn L1Pool> {
        Box::new(self.clone())
    }

    fn add_transaction(&self, transaction: L1Transaction) {
        // Do not overwrite forced transactions
        // FIXME: get rid of forced transactions and this respectively
        self.transactions
            .entry(transaction.common_data.serial_id.0)
            .or_insert(transaction);
    }

    fn get(&self, id: u64) -> Option<L1Transaction> {
        self.transactions.get(&id).map(|t| t.clone())
    }
}
