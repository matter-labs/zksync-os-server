use alloy::primitives::{B256, Bytes, U256};
use alloy::providers::{DynProvider, Provider};
use alloy::transports::{RpcError, TransportErrorKind};
use std::collections::HashMap;
use std::time::Duration;
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

    /// Forwards `eth_sendRawTransaction`, giving up after `timeout`.
    pub(crate) async fn forward_raw_transaction(
        &self,
        tx_hash: B256,
        tx_bytes: &Bytes,
        timeout: Duration,
    ) -> Result<(), TxForwardError> {
        self.forward_with_timeout(
            tx_hash,
            tx_bytes,
            TxForwardCall::SendRawTransaction,
            timeout,
        )
        .await
        .map(|_| ())
    }

    /// Forwards `eth_sendRawTransactionSync`, asking the upstream node to wait up to `max_wait`
    /// for a receipt. Passing the wait explicitly keeps the bound independent of the upstream
    /// node's own default, so the forward gives up after `max_wait + timeout`.
    pub(crate) async fn forward_raw_transaction_sync(
        &self,
        tx_hash: B256,
        tx_bytes: &Bytes,
        max_wait: Duration,
        timeout: Duration,
    ) -> Result<Option<ZkTransactionReceipt>, TxForwardError> {
        self.forward_with_timeout(
            tx_hash,
            tx_bytes,
            TxForwardCall::SendRawTransactionSync { max_wait },
            max_wait + timeout,
        )
        .await
    }

    /// Bounds the whole forward, so an upstream node that accepts the connection but never
    /// answers cannot hold the caller's request open indefinitely.
    async fn forward_with_timeout(
        &self,
        tx_hash: B256,
        tx_bytes: &Bytes,
        call: TxForwardCall,
        timeout: Duration,
    ) -> Result<Option<ZkTransactionReceipt>, TxForwardError> {
        tokio::time::timeout(timeout, self.forward(tx_hash, tx_bytes, call))
            .await
            .map_err(|_| TxForwardError::Timeout(timeout))?
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
            TxForwardCall::SendRawTransactionSync { max_wait } => Ok(Some(
                endpoint
                    .provider
                    .raw_request(
                        "eth_sendRawTransactionSync".into(),
                        (tx_bytes.clone(), U256::from(max_wait.as_millis())),
                    )
                    .await?,
            )),
        }
    }

    fn log_not_forwarding(call: TxForwardCall, tx_hash: B256) {
        match call {
            TxForwardCall::SendRawTransaction => {
                tracing::debug!(%tx_hash, "not forwarding transaction: node is leader");
            }
            TxForwardCall::SendRawTransactionSync { .. } => {
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
            (TxForwardCall::SendRawTransactionSync { .. }, Some(leader)) => {
                tracing::debug!(
                    %tx_hash,
                    leader = %leader,
                    rpc_url = %endpoint.rpc_url,
                    "forwarding sync transaction to consensus leader"
                );
            }
            (TxForwardCall::SendRawTransactionSync { .. }, None) => {
                tracing::debug!(%tx_hash, rpc_url = %endpoint.rpc_url, "forwarding sync transaction");
            }
        }
    }
}

#[derive(Clone, Copy)]
enum TxForwardCall {
    SendRawTransaction,
    SendRawTransactionSync { max_wait: Duration },
}

#[derive(Debug, thiserror::Error)]
pub enum TxForwardError {
    #[error("consensus leader is unknown")]
    NoKnownLeader,
    #[error("no RPC forwarder is configured for consensus leader {0}")]
    NoProvider(String),
    /// The upstream node may have received the transaction, so its outcome is unknown.
    #[error(
        "upstream node did not respond within {0:?}; the transaction may still be included, \
         check its status by hash before re-signing"
    )]
    Timeout(Duration),
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

    /// Whether the upstream node could not be reached or did not answer in time, as opposed
    /// to answering with a rejection. These failures are transient, so callers may retry.
    pub(crate) fn is_upstream_unavailable(&self) -> bool {
        match self {
            Self::Timeout(_) | Self::NoKnownLeader | Self::Rpc(RpcError::Transport(_)) => true,
            Self::NoProvider(_) | Self::Rpc(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::providers::ProviderBuilder;
    use tokio::net::TcpListener;

    /// Starts a TCP server that accepts connections and never answers.
    async fn blackhole_url() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                held.push(socket);
            }
        });
        format!("http://{addr}")
    }

    fn forwarder(url: &str) -> TxForwarder {
        let provider = ProviderBuilder::new()
            .connect_http(url.parse().unwrap())
            .erased();
        TxForwarder::static_target(TxForwardEndpoint::new(url.to_owned(), provider))
    }

    #[tokio::test]
    async fn forward_times_out_when_upstream_hangs() {
        let forwarder = forwarder(&blackhole_url().await);
        let timeout = Duration::from_millis(200);

        let err = forwarder
            .forward_raw_transaction(B256::ZERO, &Bytes::from_static(&[1]), timeout)
            .await
            .unwrap_err();
        assert!(matches!(err, TxForwardError::Timeout(t) if t == timeout));
        assert!(err.is_upstream_unavailable());

        let max_wait = Duration::from_millis(100);
        let err = forwarder
            .forward_raw_transaction_sync(B256::ZERO, &Bytes::from_static(&[1]), max_wait, timeout)
            .await
            .unwrap_err();
        assert!(matches!(err, TxForwardError::Timeout(t) if t == max_wait + timeout));
    }

    /// Serves one JSON-RPC request: returns its `params` and answers with an EIP-7966 timeout.
    async fn capture_one_request() -> (String, tokio::task::JoinHandle<serde_json::Value>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let body = loop {
                let mut chunk = [0u8; 4096];
                let n = socket.read(&mut chunk).await.unwrap();
                assert!(n > 0, "connection closed before the request was complete");
                buf.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buf).to_string();
                let Some((head, body)) = text.split_once("\r\n\r\n") else {
                    continue;
                };
                let len: usize = head
                    .lines()
                    .find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        k.eq_ignore_ascii_case("content-length")
                            .then(|| v.trim().parse().unwrap())
                    })
                    .unwrap();
                if body.len() >= len {
                    break body[..len].to_owned();
                }
            };
            let request: serde_json::Value = serde_json::from_str(&body).unwrap();
            let response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": request["id"],
                "error": { "code": 4, "message": "timeout" },
            })
            .to_string();
            let reply = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{response}",
                response.len()
            );
            socket.write_all(reply.as_bytes()).await.unwrap();
            request["params"].clone()
        });
        (url, handle)
    }

    #[tokio::test]
    async fn sync_forward_passes_max_wait_to_upstream() {
        let (url, request) = capture_one_request().await;
        let err = forwarder(&url)
            .forward_raw_transaction_sync(
                B256::ZERO,
                &Bytes::from_static(&[1]),
                Duration::from_millis(1500),
                Duration::from_secs(5),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err.as_rpc_error(), Some(RpcError::ErrorResp(p)) if p.code == 4),
            "{err:?}"
        );

        let params = request.await.unwrap();
        assert_eq!(params[0], "0x01");
        // 1500 ms, hex-encoded as a quantity.
        assert_eq!(params[1], "0x5dc");
    }

    #[tokio::test]
    async fn refused_connection_is_upstream_unavailable() {
        // Bind and drop to get a local port that nothing listens on.
        let addr = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap()
            .local_addr()
            .unwrap();
        let forwarder = forwarder(&format!("http://{addr}"));

        let err = forwarder
            .forward_raw_transaction(
                B256::ZERO,
                &Bytes::from_static(&[1]),
                Duration::from_secs(5),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, TxForwardError::Rpc(RpcError::Transport(_))),
            "{err:?}"
        );
        assert!(err.is_upstream_unavailable());
    }
}
