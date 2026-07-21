use alloy::primitives::{B256, Bytes};
use alloy::providers::{DynProvider, Provider};
use alloy::transports::{RpcError, TransportErrorKind};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use zksync_os_rpc_api::types::ZkTransactionReceipt;

/// Forwards transactions received over RPC to a node that can include them in a
/// block. Used on external nodes (forwarding to `main_node_rpc_url`) and on
/// consensus observers (forwarding round-robin over the validators' RPC urls).
#[derive(Clone)]
pub struct TxForwarder {
    /// Non-empty; `next` round-robins over it. Clones share the cursor, so
    /// traffic spreads across targets regardless of which clone forwards.
    endpoints: Vec<TxForwardEndpoint>,
    next: Arc<AtomicUsize>,
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
        Self::round_robin(vec![endpoint])
    }

    pub fn round_robin(endpoints: Vec<TxForwardEndpoint>) -> Self {
        assert!(
            !endpoints.is_empty(),
            "a forwarder needs at least one target"
        );
        Self {
            endpoints,
            next: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn endpoint(&self) -> &TxForwardEndpoint {
        let index = self.next.fetch_add(1, Ordering::Relaxed);
        &self.endpoints[index % self.endpoints.len()]
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
        let endpoint = self.endpoint();
        match call {
            TxForwardCall::SendRawTransaction => {
                tracing::debug!(%tx_hash, rpc_url = %endpoint.rpc_url, "forwarding transaction");
                let _ = endpoint.provider.send_raw_transaction(tx_bytes).await?;
                Ok(None)
            }
            TxForwardCall::SendRawTransactionSync => {
                tracing::debug!(%tx_hash, rpc_url = %endpoint.rpc_url, "forwarding sync transaction");
                Ok(Some(
                    endpoint
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
