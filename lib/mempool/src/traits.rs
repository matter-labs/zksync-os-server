use alloy::consensus::EthereumTxEnvelope;
use alloy::consensus::transaction::Recovered;
use alloy::primitives::TxHash;
use reth_transaction_pool::{
    EthPooledTransaction, PoolResult, PoolTransaction, TransactionOrigin, TransactionPoolExt,
};
use std::fmt::Debug;
use zksync_os_types::L2Transaction;

#[allow(async_fn_in_trait)]
#[auto_impl::auto_impl(&, Box, Arc)]
pub trait L2TransactionPool:
    TransactionPoolExt<Transaction = EthPooledTransaction> + Send + Sync + Debug + 'static
{
    /// Convenience method to add a local L2 transaction
    async fn add_l2_transaction(&self, transaction: L2Transaction) -> PoolResult<TxHash> {
        let (envelope, signer) = transaction.into_parts();
        let envelope = EthereumTxEnvelope::try_from(envelope)
            .expect("tried to insert an EIP-4844 transaction without sidecar into mempool");
        let transaction = Recovered::new_unchecked(envelope, signer);
        self.add_transaction(
            TransactionOrigin::Local,
            EthPooledTransaction::from_pooled(transaction),
        )
        .await
    }
}
