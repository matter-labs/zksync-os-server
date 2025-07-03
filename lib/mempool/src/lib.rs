mod l1_pool;
mod reth;
mod traits;

pub use crate::reth::RethPool;
pub use reth_transaction_pool::{
    CanonicalStateUpdate, PoolUpdateKind, TransactionPool as RethTransactionPool,
    TransactionPoolExt as RethTransactionPoolExt,
};
pub use traits::L2TransactionPool;

use crate::l1_pool::{L1Mempool, L1Pool};
use reth_transaction_pool::blobstore::NoopBlobStore;
use reth_transaction_pool::test_utils::OkValidator;
use reth_transaction_pool::{CoinbaseTipOrdering, PoolConfig};
use zksync_os_types::L1Transaction;

pub type DynL1Pool = Box<dyn L1Pool>;

pub fn in_memory(forced_tx: L1Transaction) -> (DynL1Pool, RethPool) {
    (
        Box::new(L1Mempool::new(forced_tx)),
        RethPool::new(
            OkValidator::default(),
            CoinbaseTipOrdering::default(),
            NoopBlobStore::default(),
            PoolConfig::default(),
        ),
    )
}
