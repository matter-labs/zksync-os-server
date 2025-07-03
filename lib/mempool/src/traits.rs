use alloy::consensus::transaction::Recovered;
use anyhow::Context;
use futures::Stream;
use futures::StreamExt;
use reth_transaction_pool::{
    EthPooledTransaction, PoolTransaction, TransactionOrigin, TransactionPoolExt,
};
use std::fmt::Debug;
use std::pin::Pin;
use zksync_os_types::{L2Envelope, L2Transaction};

#[allow(async_fn_in_trait)]
#[auto_impl::auto_impl(&, Box, Arc)]
pub trait L2TransactionPool:
    TransactionPoolExt<Transaction = EthPooledTransaction> + Send + Sync + Debug + 'static
{
    /// Convenience method to add a local L2 transaction
    async fn add_l2_transaction(&self, transaction: L2Transaction) -> anyhow::Result<()> {
        let (envelope, signer) = transaction.into_parts();
        let envelope = L2Envelope::try_from(envelope)
            .context("tried to insert an EIP4844 transaction without sidecar into mempool")?;
        let transaction = L2Transaction::new_unchecked(envelope, signer);
        self.add_transaction(
            TransactionOrigin::Local,
            EthPooledTransaction::from_pooled(transaction),
        )
        .await?;
        Ok(())
    }

    /// Convenience method to stream best L2 transactions
    fn best_l2_transactions(&self) -> Pin<Box<dyn Stream<Item = L2Transaction> + Send>> {
        Box::pin(futures::stream::iter(self.best_transactions()).map(|x| {
            let (tx, signer) = x.to_consensus().into_parts();
            let tx = L2Envelope::from(tx);
            Recovered::new_unchecked(tx, signer)
        }))
    }
}
