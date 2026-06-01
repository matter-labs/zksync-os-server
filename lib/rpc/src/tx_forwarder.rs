use alloy::primitives::{B256, Bytes};
use alloy::providers::{DynProvider, Provider};
use alloy::transports::{RpcError, TransportErrorKind};
use std::collections::HashMap;
use tokio::sync::watch;
use zksync_os_raft::RaftConsensusStatus;
use zksync_os_rpc_api::types::ZkTransactionReceipt;

/// ENs use `main_node_rpc_url`; with consensus they may hit any node, which forwards to leader.
#[derive(Clone)]
pub struct TxForwarder {
    target: TxForwardTarget,
}

#[derive(Clone)]
enum TxForwardTarget {
    /// Used on ENs: forwards to `main_node_rpc_url`.
    StaticTarget(TxForwardEndpoint),
    /// Used on consensus nodes: forwards to the current leader from Raft status.
    ConsensusLeader {
        node_id: String,
        status_rx: watch::Receiver<Option<RaftConsensusStatus>>,
        providers: HashMap<String, TxForwardEndpoint>,
    },
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

    pub fn consensus_leader(
        node_id: String,
        status_rx: watch::Receiver<Option<RaftConsensusStatus>>,
        providers: HashMap<String, TxForwardEndpoint>,
    ) -> Self {
        Self {
            target: TxForwardTarget::ConsensusLeader {
                node_id,
                status_rx,
                providers,
            },
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
                Self::forward_to_endpoint(call, tx_hash, tx_bytes, None, endpoint).await
            }
            TxForwardTarget::ConsensusLeader {
                node_id,
                status_rx,
                providers,
            } => {
                let status = status_rx.borrow().clone();
                if status.as_ref().is_some_and(|status| status.is_leader) {
                    Self::log_not_forwarding(call, tx_hash);
                    return Ok(None);
                }

                let leader = status
                    .and_then(|status| status.current_leader)
                    .ok_or(TxForwardError::NoKnownLeader)?;
                if leader == *node_id {
                    return Err(TxForwardError::NoKnownLeader);
                }
                let endpoint = providers
                    .get(&leader)
                    .ok_or_else(|| TxForwardError::NoProvider(leader.clone()))?;

                Self::forward_to_endpoint(call, tx_hash, tx_bytes, Some(&leader), endpoint).await
            }
        }
    }

    async fn forward_to_endpoint(
        call: TxForwardCall,
        tx_hash: B256,
        tx_bytes: &Bytes,
        leader: Option<&str>,
        endpoint: &TxForwardEndpoint,
    ) -> Result<Option<ZkTransactionReceipt>, TxForwardError> {
        Self::log_forwarding(call, tx_hash, leader, endpoint);

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

    fn log_not_forwarding(call: TxForwardCall, tx_hash: B256) {
        match call {
            TxForwardCall::SendRawTransaction => {
                tracing::debug!(%tx_hash, "not forwarding transaction: node is leader");
            }
            TxForwardCall::SendRawTransactionSync => {
                tracing::debug!(%tx_hash, "not forwarding sync transaction: node is leader");
            }
        }
    }

    fn log_forwarding(
        call: TxForwardCall,
        tx_hash: B256,
        leader: Option<&str>,
        endpoint: &TxForwardEndpoint,
    ) {
        match (call, leader) {
            (TxForwardCall::SendRawTransaction, Some(leader)) => {
                tracing::debug!(
                    %tx_hash,
                    leader = %leader,
                    rpc_url = %endpoint.rpc_url,
                    "forwarding transaction to consensus leader"
                );
            }
            (TxForwardCall::SendRawTransaction, None) => {
                tracing::debug!(%tx_hash, rpc_url = %endpoint.rpc_url, "forwarding transaction");
            }
            (TxForwardCall::SendRawTransactionSync, Some(leader)) => {
                tracing::debug!(
                    %tx_hash,
                    leader = %leader,
                    rpc_url = %endpoint.rpc_url,
                    "forwarding sync transaction to consensus leader"
                );
            }
            (TxForwardCall::SendRawTransactionSync, None) => {
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
    #[error("consensus leader is unknown")]
    NoKnownLeader,
    #[error("no RPC forwarder is configured for consensus leader {0}")]
    NoProvider(String),
    #[error(transparent)]
    Rpc(#[from] RpcError<TransportErrorKind>),
}

impl TxForwardError {
    pub(crate) fn as_rpc_error(&self) -> Option<&RpcError<TransportErrorKind>> {
        match self {
            Self::Rpc(err) => Some(err),
            _ => None,
        }
    }
}
