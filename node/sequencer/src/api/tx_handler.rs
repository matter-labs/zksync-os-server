use crate::reth_state::ZkClient;
use alloy::consensus::transaction::SignerRecoverable;
use alloy::eips::Decodable2718;
use alloy::primitives::{Bytes, B256};
use zksync_os_mempool::{L2TransactionPool, PoolError, RethPool};
use zksync_os_types::{L2Envelope, L2Transaction};

/// Handles transactions received in API
pub struct TxHandler {
    mempool: RethPool<ZkClient>,
}
impl TxHandler {
    pub fn new(mempool: RethPool<ZkClient>) -> TxHandler {
        Self { mempool }
    }

    pub async fn send_raw_transaction_impl(
        &self,
        tx_bytes: Bytes,
    ) -> Result<B256, EthSendRawTransactionError> {
        let transaction = L2Envelope::decode_2718(&mut tx_bytes.as_ref())
            .map_err(|_| EthSendRawTransactionError::FailedToDecodeSignedTransaction)?;
        let l2_tx: L2Transaction = transaction
            .try_into_recovered()
            .map_err(|_| EthSendRawTransactionError::InvalidTransactionSignature)?;
        let hash = *l2_tx.hash();
        self.mempool.add_l2_transaction(l2_tx).await?;

        Ok(hash)
    }
}

/// Error types returned by `eth_sendRawTransaction` implementation
#[derive(Debug, thiserror::Error)]
pub enum EthSendRawTransactionError {
    /// When decoding a signed transaction fails
    #[error("failed to decode signed transaction")]
    FailedToDecodeSignedTransaction,
    /// When the transaction signature is invalid
    #[error("invalid transaction signature")]
    InvalidTransactionSignature,
    /// Errors related to the transaction pool
    #[error(transparent)]
    PoolError(#[from] PoolError),
}
