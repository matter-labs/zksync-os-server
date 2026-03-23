use async_trait::async_trait;
use openraft::error::{Fatal, RaftError, ReplicationClosed, RPCError, StreamingError, Unreachable};
use openraft::network::{RaftNetwork, RaftNetworkFactory as RaftNetworkFactoryTrait, RPCOption};
use openraft::Config;
use openraft::raft::{
    AppendEntriesRequest, AppendEntriesResponse, InstallSnapshotRequest,
    InstallSnapshotResponse, SnapshotResponse, VoteRequest, VoteResponse,
};
use openraft::{Raft, Snapshot, Vote};
use std::collections::BTreeMap;
use std::future::Future;
use std::time::Duration;
use tokio::time::timeout;
use reth_network_peers::PeerId;
use zksync_os_consensus_types::{debug_display_raft_entry, RaftNode, RaftTypeConfig};
use zksync_os_network::raft::protocol::{RaftRequestHandler, RaftRouter};
use zksync_os_network::raft::wire::{RaftRequest, RaftResponse};

#[derive(Clone)]
pub struct RaftRpcHandler {
    raft: Raft<RaftTypeConfig>,
}

impl RaftRpcHandler {
    pub fn new(raft: Raft<RaftTypeConfig>) -> Self {
        tracing::debug!("creating raft rpc handler");
        Self { raft }
    }
}

#[async_trait]
impl RaftRequestHandler for RaftRpcHandler {
    async fn handle(&self, request: RaftRequest) -> Result<RaftResponse, String> {
        let request_kind = match &request {
            RaftRequest::AppendEntries(_) => "append_entries",
            RaftRequest::Vote(_) => "vote",
            RaftRequest::InstallSnapshot(_) => "install_snapshot",
        };
        tracing::debug!(request_kind, "handling incoming raft rpc request");
        match request {
            RaftRequest::AppendEntries(req) => {
                let resp = self
                    .raft
                    .append_entries(req)
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(RaftResponse::AppendEntries(resp))
            }
            RaftRequest::Vote(req) => {
                let resp = self.raft.vote(req).await.map_err(|e| e.to_string())?;
                Ok(RaftResponse::Vote(resp))
            }
            RaftRequest::InstallSnapshot(req) => {
                let resp = self
                    .raft
                    .install_snapshot(req)
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(RaftResponse::InstallSnapshot(resp))
            }
        }
    }
}

#[derive(Clone)]
pub struct RaftNetworkFactoryImpl {
    router: RaftRouter,
    timeout: Duration,
}

impl RaftNetworkFactoryImpl {
    pub fn new(
        router: RaftRouter,
        nodes: &BTreeMap<PeerId, RaftNode>,
        raft_config: &Config,
    ) -> anyhow::Result<Self> {
        let timeout = std::cmp::max(
            Duration::from_millis(raft_config.heartbeat_interval).saturating_mul(5),
            Duration::from_secs(2),
        );
        tracing::info!(nodes_count = nodes.len(), timeout_ms = timeout.as_millis(), "building raft network factory");
        for (node_id, node) in nodes {
            tracing::debug!(%node_id, addr = %node.addr, "registered raft network peer");
        }
        Ok(Self {
            router,
            timeout,
        })
    }
}

impl RaftNetworkFactoryTrait<RaftTypeConfig> for RaftNetworkFactoryImpl {
    type Network = RaftNetworkClient;

    fn new_client(&mut self, target: PeerId, _node: &RaftNode) -> impl Future<Output = Self::Network> + Send {
        let router = self.router.clone();
        let timeout = self.timeout;
        tracing::debug!(%target, timeout_ms = timeout.as_millis(), "creating raft network client");
        async move {
            RaftNetworkClient {
                router,
                peer_id: target,
                timeout,
            }
        }
    }
}

pub type RaftNetworkFactory = RaftNetworkFactoryImpl;

#[derive(Clone)]
pub struct RaftNetworkClient {
    router: RaftRouter,
    peer_id: PeerId,
    timeout: Duration,
}

impl RaftNetwork<RaftTypeConfig> for RaftNetworkClient {
    fn append_entries(
        &mut self,
        rpc: AppendEntriesRequest<RaftTypeConfig>,
        option: RPCOption,
    ) -> impl Future<Output = Result<AppendEntriesResponse<PeerId>, RPCError<PeerId, RaftNode, RaftError<PeerId>>>> + Send {
        let router = self.router.clone();
        let peer_id = self.peer_id;
        let timeout_dur = std::cmp::min(self.timeout, option.hard_ttl());
        async move {
            tracing::debug!(
                timeout_ms = timeout_dur.as_millis(),
                "sending raft append_entries rpc to {peer_id:?} ({} entities: {}),  prev_log_id: {}",
                rpc.entries.len(),
                rpc.entries
                    .iter()
                    .map(debug_display_raft_entry)
                    .collect::<Vec<_>>()
                    .join(", "),
                rpc.prev_log_id.map(|id| id.index).unwrap_or_default(),
            );
            let rx = router
                .send_request(peer_id, RaftRequest::AppendEntries(rpc))
                .map_err(|e| rpc_unreachable(e.to_string()))?;
            let resp_result = timeout(timeout_dur, rx)
                .await
                .map_err(|e| rpc_unreachable(e.to_string()))?
                .map_err(|e| rpc_unreachable(e.to_string()))?;
            match resp_result {
                Ok(RaftResponse::AppendEntries(resp)) => Ok(resp),
                Ok(_) => Err(rpc_unreachable("unexpected response")),
                Err(err) => Err(rpc_unreachable(err)),
            }
        }
    }

    fn vote(
        &mut self,
        rpc: VoteRequest<PeerId>,
        option: RPCOption,
    ) -> impl Future<Output = Result<VoteResponse<PeerId>, RPCError<PeerId, RaftNode, RaftError<PeerId>>>> + Send {
        let router = self.router.clone();
        let peer_id = self.peer_id;
        let timeout_dur = std::cmp::min(self.timeout, option.hard_ttl());
        async move {
            tracing::debug!(
                timeout_ms = timeout_dur.as_millis(),
                "sending raft vote rpc request to {peer_id:?} for leader {:?}",
                rpc.vote.leader_id
            );
            let rx = router
                .send_request(peer_id, RaftRequest::Vote(rpc))
                .map_err(|e| rpc_unreachable(e.to_string()))?;
            let resp_result = timeout(timeout_dur, rx)
                .await
                .map_err(|e| rpc_unreachable(e.to_string()))?
                .map_err(|e| rpc_unreachable(e.to_string()))?;
            match resp_result {
                Ok(RaftResponse::Vote(resp)) => Ok(resp),
                Ok(_) => Err(rpc_unreachable("unexpected response")),
                Err(err) => Err(rpc_unreachable(err)),
            }
        }
    }

    fn install_snapshot(
        &mut self,
        rpc: InstallSnapshotRequest<RaftTypeConfig>,
        option: RPCOption,
    ) -> impl Future<Output = Result<InstallSnapshotResponse<PeerId>, RPCError<PeerId, RaftNode, RaftError<PeerId, openraft::error::InstallSnapshotError>>>> + Send {
        let router = self.router.clone();
        let peer_id = self.peer_id;
        let client_timeout = self.timeout;
        let timeout_dur = std::cmp::min(self.timeout, option.hard_ttl());
        async move {
            tracing::debug!(
                %peer_id,
                timeout_ms = timeout_dur.as_millis(),
                option_hard_ttl_ms = option.hard_ttl().as_millis(),
                client_timeout_ms = client_timeout.as_millis(),
                "sending raft install_snapshot rpc"
            );
            let rx = router
                .send_request(peer_id, RaftRequest::InstallSnapshot(rpc))
                .map_err(|e| rpc_unreachable_install(e.to_string()))?;
            let resp_result = timeout(timeout_dur, rx)
                .await
                .map_err(|e| rpc_unreachable_install(e.to_string()))?
                .map_err(|e| rpc_unreachable_install(e.to_string()))?;
            match resp_result {
                Ok(RaftResponse::InstallSnapshot(resp)) => Ok(resp),
                Ok(_) => Err(rpc_unreachable_install("unexpected response")),
                Err(err) => Err(rpc_unreachable_install(err)),
            }
        }
    }

    fn full_snapshot(
        &mut self,
        _vote: Vote<PeerId>,
        _snapshot: Snapshot<RaftTypeConfig>,
        _cancel: impl Future<Output = ReplicationClosed> + Send + 'static,
        _option: RPCOption,
    ) -> impl Future<Output = Result<SnapshotResponse<PeerId>, StreamingError<RaftTypeConfig, Fatal<PeerId>>>> + Send {
        async move {
            let err = std::io::Error::new(std::io::ErrorKind::Other, "snapshotting disabled");
            Err(StreamingError::Unreachable(Unreachable::new(&err)))
        }
    }
}

fn rpc_unreachable(msg: impl ToString) -> RPCError<PeerId, RaftNode, RaftError<PeerId>> {
    let err = std::io::Error::new(std::io::ErrorKind::Other, msg.to_string());
    RPCError::Unreachable(Unreachable::new(&err))
}

fn rpc_unreachable_install(
    msg: impl ToString,
) -> RPCError<PeerId, RaftNode, RaftError<PeerId, openraft::error::InstallSnapshotError>> {
    let err = std::io::Error::new(std::io::ErrorKind::Other, msg.to_string());
    RPCError::Unreachable(Unreachable::new(&err))
}
