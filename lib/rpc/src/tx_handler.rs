use alloy::consensus::transaction::SignerRecoverable;
use alloy::eips::Decodable2718;
use alloy::primitives::{Address, B256, Bytes};
use alloy::providers::{DynProvider, Provider};
use alloy::transports::{RpcError, TransportErrorKind};
use std::collections::HashSet;
use tokio::sync::watch;
use zksync_os_mempool::{L2TransactionPool, PoolError};
use zksync_os_types::{L2Envelope, L2Transaction, NotAcceptingReason, TransactionAcceptanceState};

/// Handles transactions received in API
pub struct TxHandler<Mempool> {
    mempool: Mempool,
    acceptance_state: watch::Receiver<TransactionAcceptanceState>,
    l2_signer_blacklist: HashSet<Address>,
    tx_forwarder: Option<DynProvider>,
}

impl<Mempool: L2TransactionPool> TxHandler<Mempool> {
    pub fn new(
        mempool: Mempool,
        acceptance_state: watch::Receiver<TransactionAcceptanceState>,
        l2_signer_blacklist: HashSet<Address>,
        tx_forwarder: Option<DynProvider>,
    ) -> Self {
        Self {
            mempool,
            acceptance_state,
            l2_signer_blacklist,
            tx_forwarder,
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
            return Err(EthSendRawTransactionError::BlacklistedSigner);
        }
        self.mempool.add_l2_transaction(l2_tx).await?;

        if let Some(tx_forwarder) = self.tx_forwarder.as_ref() {
            match tx_forwarder.send_raw_transaction(&tx_bytes).await {
                // We do not need to wait for pending transaction here, so it's safe to forget about it
                Ok(_) => {}
                Err(err) => {
                    tracing::debug!(%err, "forwarding error from main node back to user");
                    // Remove previously added transaction from local mempool
                    self.mempool.remove_transactions(vec![hash]);
                    return Err(err.into());
                }
            }
        }

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
    /// Error forwarded from main node
    #[error(transparent)]
    ForwardError(#[from] RpcError<TransportErrorKind>),
    #[error("Signer is blacklisted")]
    BlacklistedSigner,
}
