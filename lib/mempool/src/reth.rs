use crate::traits::L2TransactionPool;
use reth_transaction_pool::blobstore::NoopBlobStore;
use reth_transaction_pool::test_utils::OkValidator;
use reth_transaction_pool::{CoinbaseTipOrdering, EthPooledTransaction, Pool};

pub type RethPool = Pool<OkValidator, CoinbaseTipOrdering<EthPooledTransaction>, NoopBlobStore>;

impl L2TransactionPool for RethPool {}
