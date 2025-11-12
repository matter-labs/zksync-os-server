use alloy::consensus::transaction::SignerRecoverable;
use alloy::eips::Decodable2718;
use alloy::primitives::{Address, B256, Bytes};
use std::collections::HashSet;
use tokio::sync::watch;
use zksync_os_mempool::{L2TransactionPool, PoolError};
use zksync_os_types::{L2Envelope, L2Transaction, NotAcceptingReason, TransactionAcceptanceState};

/// Handles transactions received in API
pub struct TxHandler<Mempool> {
    mempool: Mempool,
    acceptance_state: watch::Receiver<TransactionAcceptanceState>,
    l2_signer_blacklist: HashSet<Address>,
}

impl<Mempool: L2TransactionPool> TxHandler<Mempool> {
    pub fn new(
        mempool: Mempool,
        acceptance_state: watch::Receiver<TransactionAcceptanceState>,
        l2_signer_blacklist: HashSet<Address>,
    ) -> Self {
        Self {
            mempool,
            acceptance_state,
            l2_signer_blacklist,
        }
    }

    pub async fn send_raw_transaction_impl(
        &self,
        tx_bytes: Bytes,
    ) -> Result<B256, EthSendRawTransactionError> {
        if let TransactionAcceptanceState::NotAccepting(reason) = &*self.acceptance_state.borrow() {
            return Err(EthSendRawTransactionError::NotAcceptingTransactions(
                *reason,
            ));
        }

        let transaction = L2Envelope::decode_2718(&mut tx_bytes.as_ref())
            .map_err(|_| EthSendRawTransactionError::FailedToDecodeSignedTransaction)?;
        let l2_tx: L2Transaction = transaction
            .try_into_recovered()
            .map_err(|_| EthSendRawTransactionError::InvalidTransactionSignature)?;
        let hash = *l2_tx.hash();
        if self.l2_signer_blacklist.contains(&l2_tx.signer()) {
            return Err(EthSendRawTransactionError::NotAcceptingTransactions(
                NotAcceptingReason::BlacklistedSigner,
            ));
        }
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
    /// When the node is not accepting new transactions
    #[error(transparent)]
    NotAcceptingTransactions(NotAcceptingReason),
    /// Errors related to the transaction pool
    #[error(transparent)]
    PoolError(#[from] PoolError),
}
