use crate::eth_impl::build_api_receipt;
use crate::{ReadRpcStorage, RpcConfig};
use alloy::consensus::transaction::SignerRecoverable;
use alloy::eips::Decodable2718;
use alloy::primitives::{B256, Bytes, U256};
use alloy::providers::{DynProvider, Provider};
use alloy::transports::{RpcError, TransportErrorKind};
use std::time::Duration;
use tokio::sync::watch;
use zksync_os_mempool::{L2TransactionPool, PoolError};
use zksync_os_rpc_api::types::ZkTransactionReceipt;
use zksync_os_types::{L2Envelope, L2Transaction, NotAcceptingReason, TransactionAcceptanceState};

/// Maximum user provided timeout for `eth_sendRawTransactionSync`. Chosen liberally as waiting is
/// inexpensive.
const SEND_RAW_TRANSACTION_SYNC_MAX_TIMEOUT: Duration = Duration::from_secs(30);

/// Handles transactions received in API
pub struct TxHandler<RpcStorage, Mempool> {
    config: RpcConfig,
    storage: RpcStorage,
    mempool: Mempool,
    acceptance_state: watch::Receiver<TransactionAcceptanceState>,
    tx_forwarder: Option<DynProvider>,
}

impl<RpcStorage: ReadRpcStorage, Mempool: L2TransactionPool> TxHandler<RpcStorage, Mempool> {
    pub fn new(
        config: RpcConfig,
        storage: RpcStorage,
        mempool: Mempool,
        acceptance_state: watch::Receiver<TransactionAcceptanceState>,
        tx_forwarder: Option<DynProvider>,
    ) -> Self {
        Self {
            config,
            storage,
            mempool,
            acceptance_state,
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
        if self.config.l2_signer_blacklist.contains(&l2_tx.signer()) {
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

    pub async fn send_raw_transaction_sync_impl(
        &self,
        bytes: Bytes,
        max_wait_ms: Option<U256>,
    ) -> Result<ZkTransactionReceipt, EthSendRawTransactionSyncError> {
        let timeout_duration = if let Some(timeout_ms) = max_wait_ms {
            match timeout_ms.try_into() {
                Ok(timeout_u64) => {
                    let requested_timeout = Duration::from_millis(timeout_u64);
                    if requested_timeout > SEND_RAW_TRANSACTION_SYNC_MAX_TIMEOUT {
                        // Per EIP-7966 MUST use default timeout if user provided timeout is invalid
                        self.config.send_raw_transaction_sync_timeout
                    } else {
                        requested_timeout
                    }
                }
                Err(_) => {
                    // Per EIP-7966 MUST use default timeout if user provided timeout is invalid
                    self.config.send_raw_transaction_sync_timeout
                }
            }
        } else {
            self.config.send_raw_transaction_sync_timeout
        };

        // Create block subscription
        let mut block_rx = self.storage.block_subscriptions().subscribe_to_blocks();

        let tx_hash = self.send_raw_transaction_impl(bytes).await?;

        // Wait for the transaction to appear in a block or timeout
        tokio::time::timeout(timeout_duration, async {
            loop {
                // Wait for the next block notification
                let Ok(block) = block_rx.recv().await else {
                    // Channel closed or is lagging, this shouldn't happen in normal operation
                    tracing::warn!("block subscription closed while waiting for tx receipt");
                    return Err(EthSendRawTransactionSyncError::Timeout(timeout_duration));
                };

                if let Some(stored_tx) = block.transactions.get(&tx_hash) {
                    return Ok(build_api_receipt(
                        tx_hash,
                        stored_tx.receipt.clone(),
                        &stored_tx.tx,
                        &stored_tx.meta,
                    ));
                }
            }
        })
        .await
        .map_err(|_| EthSendRawTransactionSyncError::Timeout(timeout_duration))?
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

/// Error types returned by `eth_sendRawTransactionSync` implementation
#[derive(Debug, thiserror::Error)]
pub enum EthSendRawTransactionSyncError {
    /// Regular `eth_sendRawTransaction` errors
    #[error(transparent)]
    Regular(#[from] EthSendRawTransactionError),
    /// Timeout while waiting for transaction receipt.
    #[error("The transaction was added to the mempool but wasn't processed within {0:?}.")]
    Timeout(Duration),
}
