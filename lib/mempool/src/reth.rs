use crate::L2PooledTransaction;
use crate::traits::L2TransactionPool;
use reth_transaction_pool::blobstore::NoopBlobStore;
use reth_transaction_pool::test_utils::OkValidator;
use reth_transaction_pool::{CoinbaseTipOrdering, Pool};

pub type RethPool =
    Pool<OkValidator<L2PooledTransaction>, CoinbaseTipOrdering<L2PooledTransaction>, NoopBlobStore>;

impl L2TransactionPool for RethPool {}
