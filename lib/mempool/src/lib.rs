mod pool;
mod traits;

use crate::pool::Mempool;
pub use traits::TransactionPool;
use zksync_types::Transaction;

pub type DynPool = Box<dyn TransactionPool>;

pub fn in_memory(forced_tx: Transaction) -> DynPool {
    Box::new(Mempool::new(forced_tx))
}
