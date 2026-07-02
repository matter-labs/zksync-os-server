use alloy::primitives::{B256, Bytes};
use alloy::providers::{DynProvider, Provider};
use alloy::transports::{RpcError, TransportErrorKind};
use zksync_os_rpc_api::types::ZkTransactionReceipt;

/// Forwards transactions received over RPC to the node that can include them in a block.
/// Used on external nodes, which forward to `main_node_rpc_url`.
#[derive(Clone)]
pub struct TxForwarder {
    endpoint: TxForwardEndpoint,
}

#[derive(Clone)]
pub struct TxForwardEndpoint {
    rpc_url: String,
    provider: DynProvider,
}

impl TxForwardEndpoint {
    pub fn new(rpc_url: String, provider: DynProvider) -> Self {
        Self { rpc_url, provider }
    }
}

impl TxForwarder {
    pub fn static_target(endpoint: TxForwardEndpoint) -> Self {
        Self { endpoint }
    }

    pub(crate) async fn forward_raw_transaction(
        &self,
        tx_hash: B256,
        tx_bytes: &Bytes,
    ) -> Result<(), TxForwardError> {
        self.forward(tx_hash, tx_bytes, TxForwardCall::SendRawTransaction)
            .await
            .map(|_| ())
    }

    pub(crate) async fn forward_raw_transaction_sync(
        &self,
        tx_hash: B256,
        tx_bytes: &Bytes,
    ) -> Result<Option<ZkTransactionReceipt>, TxForwardError> {
        self.forward(tx_hash, tx_bytes, TxForwardCall::SendRawTransactionSync)
            .await
    }

    async fn forward(
        &self,
        tx_hash: B256,
        tx_bytes: &Bytes,
        call: TxForwardCall,
    ) -> Result<Option<ZkTransactionReceipt>, TxForwardError> {
        match call {
            TxForwardCall::SendRawTransaction => {
                tracing::debug!(%tx_hash, rpc_url = %self.endpoint.rpc_url, "forwarding transaction");
                let _ = self
                    .endpoint
                    .provider
                    .send_raw_transaction(tx_bytes)
                    .await?;
                Ok(None)
            }
            TxForwardCall::SendRawTransactionSync => {
                tracing::debug!(%tx_hash, rpc_url = %self.endpoint.rpc_url, "forwarding sync transaction");
                Ok(Some(
                    self.endpoint
                        .provider
                        .raw_request("eth_sendRawTransactionSync".into(), (tx_bytes.clone(),))
                        .await?,
                ))
            }
        }
    }
}

#[derive(Clone, Copy)]
enum TxForwardCall {
    SendRawTransaction,
    SendRawTransactionSync,
}

#[derive(Debug, thiserror::Error)]
pub enum TxForwardError {
    #[error(transparent)]
    Rpc(#[from] RpcError<TransportErrorKind>),
}

impl TxForwardError {
    pub(crate) fn as_rpc_error(&self) -> Option<&RpcError<TransportErrorKind>> {
        match self {
            Self::Rpc(err) => Some(err),
        }
    }
}
