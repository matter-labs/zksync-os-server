use async_trait::async_trait;
use openraft::Config;
use openraft::error::{Fatal, RPCError, RaftError, ReplicationClosed, StreamingError, Unreachable};
use openraft::network::{RPCOption, RaftNetwork, RaftNetworkFactory as OpenraftNetworkFactory};
use openraft::raft::{
    AppendEntriesRequest, AppendEntriesResponse, InstallSnapshotRequest, InstallSnapshotResponse,
    SnapshotResponse, VoteRequest, VoteResponse,
};
use openraft::{Raft, Snapshot, Vote};
use reth_network_peers::PeerId;
use std::collections::BTreeMap;
use std::future::Future;
use std::time::Duration;
use tokio::time::timeout;
use zksync_os_consensus_types::{RaftNode, RaftTypeConfig, debug_display_raft_entry};
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
        tracing::debug!("handling incoming raft rpc request ({request})");
        match request {
            RaftRequest::AppendEntries(r) => self
                .raft
                .append_entries(r)
                .await
                .map(RaftResponse::AppendEntries)
                .map_err(|e| e.to_string()),
            RaftRequest::Vote(r) => self
                .raft
                .vote(r)
                .await
                .map(RaftResponse::Vote)
                .map_err(|e| e.to_string()),
            RaftRequest::InstallSnapshot(r) => {
                #[allow(deprecated)]
                let response = self.raft.install_snapshot(r).await;
                response
                    .map(RaftResponse::InstallSnapshot)
                    .map_err(|e| e.to_string())
            }
        }
    }
}

#[derive(Clone)]
pub struct RaftNetworkFactory {
    router: RaftRouter,
    timeout: Duration,
}

impl RaftNetworkFactory {
    pub fn new(
        router: RaftRouter,
        nodes: &BTreeMap<PeerId, RaftNode>,
        raft_config: &Config,
    ) -> anyhow::Result<Self> {
        let timeout = std::cmp::max(
            Duration::from_millis(raft_config.heartbeat_interval).saturating_mul(5),
            Duration::from_secs(2),
        );
        tracing::info!(
            "building raft network factory (nodes_count={}, timeout_ms={})",
            nodes.len(),
            timeout.as_millis()
        );
        for (node_id, node) in nodes {
            tracing::debug!(
                "registered raft network peer: {node_id}, addr={}",
                node.addr
            );
        }
        Ok(Self { router, timeout })
    }
}

impl OpenraftNetworkFactory<RaftTypeConfig> for RaftNetworkFactory {
    type Network = RaftNetworkClient;

    async fn new_client(&mut self, target: PeerId, _node: &RaftNode) -> Self::Network {
        tracing::debug!(
            "creating raft network client: target={target}, timeout_ms={}",
            self.timeout.as_millis()
        );
        RaftNetworkClient {
            router: self.router.clone(),
            peer_id: target,
            timeout: self.timeout,
        }
    }
}

#[derive(Clone)]
pub struct RaftNetworkClient {
    router: RaftRouter,
    peer_id: PeerId,
    timeout: Duration,
}

impl RaftNetworkClient {
    async fn send_rpc<E: std::error::Error>(
        &self,
        request: RaftRequest,
        timeout_dur: Duration,
    ) -> Result<RaftResponse, RPCError<PeerId, RaftNode, E>> {
        let rx = self
            .router
            .send_request(self.peer_id, request)
            .map_err(|e| rpc_unreachable(e.to_string()))?; // peer not connected
        timeout(timeout_dur, rx)
            .await
            .map_err(|e| rpc_unreachable(e.to_string()))? // timeout elapsed
            .map_err(|e| rpc_unreachable(e.to_string()))? // response channel dropped
            .map_err(rpc_unreachable) // handler returned Err
    }
}

impl RaftNetwork<RaftTypeConfig> for RaftNetworkClient {
    async fn append_entries(
        &mut self,
        rpc: AppendEntriesRequest<RaftTypeConfig>,
        option: RPCOption,
    ) -> Result<AppendEntriesResponse<PeerId>, RPCError<PeerId, RaftNode, RaftError<PeerId>>> {
        let timeout_dur = std::cmp::min(self.timeout, option.hard_ttl());
        tracing::debug!(
            "sending raft append_entries rpc to {:?} ({} entries: {}), prev_log_id: {}, timeout_ms={}",
            self.peer_id,
            rpc.entries.len(),
            rpc.entries
                .iter()
                .map(debug_display_raft_entry)
                .collect::<Vec<_>>()
                .join(", "),
            rpc.prev_log_id.map(|id| id.index).unwrap_or_default(),
            timeout_dur.as_millis(),
        );
        match self
            .send_rpc(RaftRequest::AppendEntries(rpc), timeout_dur)
            .await?
        {
            RaftResponse::AppendEntries(r) => Ok(r),
            other => unreachable!("append_entries rpc returned wrong response variant: {other:?}"),
        }
    }

    async fn vote(
        &mut self,
        rpc: VoteRequest<PeerId>,
        option: RPCOption,
    ) -> Result<VoteResponse<PeerId>, RPCError<PeerId, RaftNode, RaftError<PeerId>>> {
        let timeout_dur = std::cmp::min(self.timeout, option.hard_ttl());
        tracing::debug!(
            "sending raft vote rpc to {:?} for leader {:?} (timeout_ms={})",
            self.peer_id,
            rpc.vote.leader_id,
            timeout_dur.as_millis(),
        );
        match self.send_rpc(RaftRequest::Vote(rpc), timeout_dur).await? {
            RaftResponse::Vote(r) => Ok(r),
            other => unreachable!("vote rpc returned wrong response variant: {other:?}"),
        }
    }

    async fn install_snapshot(
        &mut self,
        rpc: InstallSnapshotRequest<RaftTypeConfig>,
        option: RPCOption,
    ) -> Result<
        InstallSnapshotResponse<PeerId>,
        RPCError<PeerId, RaftNode, RaftError<PeerId, openraft::error::InstallSnapshotError>>,
    > {
        let timeout_dur = std::cmp::min(self.timeout, option.hard_ttl());
        tracing::debug!(
            "sending raft install_snapshot rpc to {} (timeout_ms={})",
            self.peer_id,
            timeout_dur.as_millis(),
        );
        match self
            .send_rpc(RaftRequest::InstallSnapshot(rpc), timeout_dur)
            .await?
        {
            RaftResponse::InstallSnapshot(r) => Ok(r),
            other => {
                unreachable!("install_snapshot rpc returned wrong response variant: {other:?}")
            }
        }
    }

    async fn full_snapshot(
        &mut self,
        _vote: Vote<PeerId>,
        _snapshot: Snapshot<RaftTypeConfig>,
        _cancel: impl Future<Output = ReplicationClosed> + Send + 'static,
        _option: RPCOption,
    ) -> Result<SnapshotResponse<PeerId>, StreamingError<RaftTypeConfig, Fatal<PeerId>>> {
        let err = std::io::Error::other("snapshotting disabled");
        Err(StreamingError::Unreachable(Unreachable::new(&err)))
    }
}

fn rpc_unreachable<E: std::error::Error>(msg: impl ToString) -> RPCError<PeerId, RaftNode, E> {
    let err = std::io::Error::other(msg.to_string());
    RPCError::Unreachable(Unreachable::new(&err))
}
