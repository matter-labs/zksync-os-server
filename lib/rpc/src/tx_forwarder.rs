use alloy::primitives::{B256, Bytes};
use alloy::providers::{DynProvider, Provider};
use alloy::transports::{RpcError, TransportErrorKind};
use zksync_os_rpc_api::types::ZkTransactionReceipt;

/// Forwards external-node transactions to `main_node_rpc_url`.
#[derive(Clone)]
pub struct TxForwarder {
    target: TxForwardTarget,
}

#[derive(Clone)]
enum TxForwardTarget {
    /// Used on ENs: forwards to `main_node_rpc_url`.
    StaticTarget(TxForwardEndpoint),
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
        Self {
            target: TxForwardTarget::StaticTarget(endpoint),
        }
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
        match &self.target {
            TxForwardTarget::StaticTarget(endpoint) => {
                Self::forward_to_endpoint(call, tx_hash, tx_bytes, endpoint).await
            }
        }
    }

    async fn forward_to_endpoint(
        call: TxForwardCall,
        tx_hash: B256,
        tx_bytes: &Bytes,
        endpoint: &TxForwardEndpoint,
    ) -> Result<Option<ZkTransactionReceipt>, TxForwardError> {
        Self::log_forwarding(call, tx_hash, endpoint);

        match call {
            TxForwardCall::SendRawTransaction => {
                let _ = endpoint.provider.send_raw_transaction(tx_bytes).await?;
                Ok(None)
            }
            TxForwardCall::SendRawTransactionSync => Ok(Some(
                endpoint
                    .provider
                    .raw_request("eth_sendRawTransactionSync".into(), (tx_bytes.clone(),))
                    .await?,
            )),
        }
    }

    fn log_forwarding(call: TxForwardCall, tx_hash: B256, endpoint: &TxForwardEndpoint) {
        match call {
            TxForwardCall::SendRawTransaction => {
                tracing::debug!(%tx_hash, rpc_url = %endpoint.rpc_url, "forwarding transaction");
            }
            TxForwardCall::SendRawTransactionSync => {
                tracing::debug!(%tx_hash, rpc_url = %endpoint.rpc_url, "forwarding sync transaction");
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
