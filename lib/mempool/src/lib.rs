mod l1_pool;
mod pool;
mod traits;

use crate::l1_pool::{L1Mempool, L1Pool};
use crate::pool::Mempool;
pub use traits::TransactionPool;
use zksync_os_types::L1Transaction;

pub type DynPool = Box<dyn TransactionPool>;
pub type DynL1Pool = Box<dyn L1Pool>;

pub fn in_memory(forced_tx: L1Transaction) -> (DynL1Pool, DynPool) {
    (
        Box::new(L1Mempool::new(forced_tx)),
        Box::new(Mempool::new()),
    )
}
