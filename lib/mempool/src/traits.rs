use futures::Stream;
use std::fmt::Debug;
use zksync_os_types::L2Transaction;

#[auto_impl::auto_impl(&, Box, Arc)]
pub trait TransactionPool: Stream<Item = L2Transaction> + Send + Sync + Debug + 'static {
    /// Alternative for [`Clone::clone`] that is object safe.
    fn dyn_clone(&self) -> Box<dyn TransactionPool>;

    fn add_transaction(&self, transaction: L2Transaction);
}

impl Clone for Box<dyn TransactionPool> {
    fn clone(&self) -> Self {
        self.dyn_clone()
    }
}
