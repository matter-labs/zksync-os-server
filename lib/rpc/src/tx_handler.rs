use crate::eth_impl::build_api_receipt;
use crate::metrics::{TX_SUBMISSION, TxRejectionReason};
use crate::{ReadRpcStorage, RpcConfig};
use alloy::consensus::transaction::SignerRecoverable;
use alloy::eips::Decodable2718;
use alloy::primitives::{B256, Bytes, U256};
use alloy::providers::{DynProvider, Provider};
use alloy::transports::{RpcError, TransportErrorKind};
use std::time::{Duration, Instant};
use tokio::sync::watch;
use zksync_os_interface::error::InvalidTransaction;
use zksync_os_interface::types::BlockContext;
use zksync_os_mempool::PoolError;
use zksync_os_mempool::subpools::l2::L2Subpool;
use zksync_os_mempool::{InvalidPoolTransactionError, PoolErrorKind};
use zksync_os_rpc_api::types::ZkTransactionReceipt;
use zksync_os_tx_validators::policy_client::{AccessType, PolicyClient};
use zksync_os_types::{
    L2Envelope, L2Transaction, NotAcceptingReason, TransactionAcceptanceState, ZkTransaction,
};

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
    /// Optional policy client. When set, each incoming tx is simulated
    /// once with the validator wired in (admit + judge inline). Spares
    /// clients a `pending → no receipt` poll loop on a stable deny.
    /// Block-build remains authoritative.
    policy_client: Option<PolicyClient>,
    /// Latest block context constructed by the sequencer; used as the
    /// simulation environment for the RPC-side judge call. `None` until
    /// the sequencer has built at least one block.
    last_constructed_block_context: watch::Receiver<Option<BlockContext>>,
}

impl<RpcStorage: ReadRpcStorage, Mempool: L2Subpool> TxHandler<RpcStorage, Mempool> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: RpcConfig,
        storage: RpcStorage,
        mempool: Mempool,
        acceptance_state: watch::Receiver<TransactionAcceptanceState>,
        tx_forwarder: Option<DynProvider>,
        policy_client: Option<PolicyClient>,
        last_constructed_block_context: watch::Receiver<Option<BlockContext>>,
    ) -> Self {
        Self {
            config,
            storage,
            mempool,
            acceptance_state,
            tx_forwarder,
            policy_client,
            last_constructed_block_context,
        }
    }

    pub async fn send_raw_transaction_impl(
        &self,
        tx_bytes: Bytes,
    ) -> Result<B256, EthSendRawTransactionError> {
        if let TransactionAcceptanceState::NotAccepting(reasons) = &*self.acceptance_state.borrow()
        {
            return Err(EthSendRawTransactionError::NotAcceptingTransactions(
                reasons.clone(),
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

        if let Some(policy_client) = &self.policy_client {
            // Skip the simulation until the sequencer has built at least
            // one block. Block-build runs the policy authoritatively then.
            // Copy the watch ref before await so the future stays `Send`.
            let block_context = *self.last_constructed_block_context.borrow();
            if let Some(block_context) = block_context {
                let storage_view = self
                    .storage
                    .state_at_block_number_or_latest(block_context.block_number)
                    .map_err(|err| EthSendRawTransactionError::JudgeSimFailed(err.into()))?;
                let zk_tx: ZkTransaction = l2_tx.clone().into();
                let policy_client = policy_client.clone();
                // `spawn_blocking`: the sim is CPU-bound and the validator
                // hooks call `Handle::block_on` internally, which would
                // panic on a runtime worker thread. Note: dropping the
                // outer future (RPC client disconnect) does not cancel
                // this task; admit and judge fire to completion.
                let sim = tokio::task::spawn_blocking(move || {
                    let mut policy_session = policy_client.session(AccessType::Write);
                    let mut tracer = policy_session.paired_tracer();
                    crate::sandbox::execute_with(
                        zk_tx,
                        block_context,
                        storage_view,
                        &mut tracer,
                        &mut policy_session,
                    )
                })
                .await
                .map_err(|err| EthSendRawTransactionError::JudgeSimFailed(err.into()))?
                .map_err(EthSendRawTransactionError::JudgeSimFailed)?;
                if let Err(err) = sim
                    && matches!(err, InvalidTransaction::FilteredByValidator)
                {
                    return Err(EthSendRawTransactionError::PolicyDenied);
                }
                // Other sim errors (nonce, gas, etc.) are handled by the
                // mempool / block-build rejection paths.
            }
        }
        {
            let _guard = MempoolLatencyGuard::new();
            self.mempool.add_l2_transaction(l2_tx).await?;
        }

        if let Some(tx_forwarder) = self.tx_forwarder.as_ref() {
            let forwarding_result = {
                let _guard = ForwardingLatencyGuard::new();
                tx_forwarder.send_raw_transaction(&tx_bytes).await
            };
            // We do not need to wait for pending transaction here, so it's safe to forget about it
            if let Err(err) = forwarding_result {
                tracing::debug!(%err, "forwarding error from main node back to user");
                // Remove previously added transaction from local mempool
                self.mempool.remove_transactions(vec![hash]);
                return Err(err.into());
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
    #[error("{}", .0.iter().map(|r| r.to_string()).collect::<Vec<_>>().join("; "))]
    NotAcceptingTransactions(Vec<NotAcceptingReason>),
    /// Errors related to the transaction pool
    #[error(transparent)]
    PoolError(#[from] PoolError),
    /// Error forwarded from main node
    #[error(transparent)]
    ForwardError(#[from] RpcError<TransportErrorKind>),
    #[error("Signer is blacklisted")]
    BlacklistedSigner,
    /// Policy service rejected the transaction.
    #[error("transaction denied by policy service")]
    PolicyDenied,
    /// Local simulation for the RPC-side judge call failed for an internal
    /// reason (storage error, etc.). Clean tx rejections fall through and
    /// surface via the mempool / block-build paths instead.
    #[error("failed to simulate transaction: {0}")]
    JudgeSimFailed(#[source] anyhow::Error),
}

impl From<&EthSendRawTransactionError> for TxRejectionReason {
    fn from(err: &EthSendRawTransactionError) -> Self {
        match err {
            EthSendRawTransactionError::FailedToDecodeSignedTransaction => Self::DecodeFailed,
            EthSendRawTransactionError::InvalidTransactionSignature => Self::InvalidSignature,
            EthSendRawTransactionError::NotAcceptingTransactions(_) => Self::NotAccepting,
            EthSendRawTransactionError::BlacklistedSigner => Self::BlacklistedSigner,
            EthSendRawTransactionError::ForwardError(rpc_err) => match rpc_err {
                RpcError::ErrorResp(_) => Self::ForwardRejected,
                _ => Self::ForwardTransportError,
            },
            EthSendRawTransactionError::PoolError(pool_err) => Self::from(&pool_err.kind),
            EthSendRawTransactionError::PolicyDenied => Self::PolicyDenied,
            EthSendRawTransactionError::JudgeSimFailed(_) => Self::JudgeSimFailed,
        }
    }
}

impl From<&PoolErrorKind> for TxRejectionReason {
    fn from(kind: &PoolErrorKind) -> Self {
        match kind {
            PoolErrorKind::AlreadyImported => Self::PoolAlreadyImported,
            PoolErrorKind::ReplacementUnderpriced => Self::PoolReplacementUnderpriced,
            PoolErrorKind::FeeCapBelowMinimumProtocolFeeCap(_) => Self::PoolFeeCapBelowMinimum,
            PoolErrorKind::SpammerExceededCapacity(_) => Self::PoolSpammerExceededCapacity,
            PoolErrorKind::DiscardedOnInsert => Self::PoolDiscardedOnInsert,
            PoolErrorKind::ExistingConflictingTransactionType(_, _) => Self::PoolConflictingTxType,
            PoolErrorKind::InvalidTransaction(invalid) => Self::from(invalid),
            PoolErrorKind::Other(_) => Self::PoolOther,
        }
    }
}

impl From<&InvalidPoolTransactionError> for TxRejectionReason {
    fn from(err: &InvalidPoolTransactionError) -> Self {
        match err {
            InvalidPoolTransactionError::Consensus(_) => Self::PoolConsensusError,
            InvalidPoolTransactionError::ExceedsGasLimit(_, _) => Self::PoolExceedsGasLimit,
            InvalidPoolTransactionError::MaxTxGasLimitExceeded(_, _) => {
                Self::PoolMaxTxGasLimitExceeded
            }
            InvalidPoolTransactionError::ExceedsFeeCap { .. } => Self::PoolExceedsFeeCap,
            InvalidPoolTransactionError::ExceedsMaxInitCodeSize(_, _) => {
                Self::PoolExceedsMaxInitCodeSize
            }
            InvalidPoolTransactionError::OversizedData { .. } => Self::PoolOversizedData,
            InvalidPoolTransactionError::Underpriced => Self::PoolUnderpriced,
            InvalidPoolTransactionError::Overdraft { .. } => Self::PoolOverdraft,
            InvalidPoolTransactionError::Eip2681 => Self::PoolNonceOverflow,
            InvalidPoolTransactionError::Eip4844(_) => Self::PoolEip4844Error,
            InvalidPoolTransactionError::Eip7702(_) => Self::PoolEip7702Error,
            InvalidPoolTransactionError::Other(_) => Self::PoolOther,
            InvalidPoolTransactionError::IntrinsicGasTooLow => Self::PoolIntrinsicGasTooLow,
            InvalidPoolTransactionError::PriorityFeeBelowMinimum { .. } => {
                Self::PoolPriorityFeeBelowMinimum
            }
        }
    }
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

/// Records mempool insertion latency on drop, capturing errors and async cancellations.
struct MempoolLatencyGuard(Instant);

impl MempoolLatencyGuard {
    fn new() -> Self {
        Self(Instant::now())
    }
}

impl Drop for MempoolLatencyGuard {
    fn drop(&mut self) {
        TX_SUBMISSION.mempool_latency.observe(self.0.elapsed());
    }
}

/// Records forwarding latency on drop, capturing errors and async cancellations.
struct ForwardingLatencyGuard(Instant);

impl ForwardingLatencyGuard {
    fn new() -> Self {
        Self(Instant::now())
    }
}

impl Drop for ForwardingLatencyGuard {
    fn drop(&mut self) {
        TX_SUBMISSION.forwarding_latency.observe(self.0.elapsed());
    }
}
