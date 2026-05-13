use openraft::Raft;
use reth_network_peers::PeerId;
use std::collections::BTreeMap;
use std::time::Duration;
use zksync_os_consensus_types::{RaftNode, RaftTypeConfig};
use zksync_os_network::raft::protocol::RaftRouter;

/// One-shot bootstrapper for a node that may initialize a new raft cluster.
///
/// Nodes with bootstrap enabled wait until all other members have established devp2p connections,
/// then call `raft.initialize()` to form the initial membership. It is valid for every consensus
/// node to do this: the first initializer wins and the others skip once the cluster is initialized.
///
/// If the cluster is already initialized (e.g. after a restart), `bootstrap_if_needed` is a
/// no-op.
pub struct RaftBootstrapper {
    pub(crate) raft: Raft<RaftTypeConfig>,
    pub(crate) router: RaftRouter,
    pub(crate) node_id: PeerId,
    pub(crate) peer_ids: Vec<PeerId>,
    pub(crate) membership_nodes: BTreeMap<PeerId, RaftNode>,
}

impl RaftBootstrapper {
    pub async fn bootstrap_if_needed(&self) -> anyhow::Result<()> {
        const BOOTSTRAP_WAIT_RETRY: Duration = Duration::from_secs(30);

        if self.raft.is_initialized().await? {
            tracing::info!("raft cluster is already initialized; skipping bootstrap process");
            return Ok(());
        }

        let required_peers: Vec<_> = self
            .peer_ids
            .iter()
            .copied()
            .filter(|peer_id| *peer_id != self.node_id)
            .collect();
        if !required_peers.is_empty() {
            tracing::info!("waiting for raft peers to connect: {required_peers:?}");
            loop {
                match self
                    .router
                    .wait_for_peers(&required_peers, BOOTSTRAP_WAIT_RETRY)
                    .await
                {
                    Ok(()) => break,
                    Err(missing) => {
                        tracing::info!(
                            "still waiting for raft peers before bootstrap: missing={missing:?}, connected={:?}",
                            self.router.connected_peers()
                        );
                    }
                }
            }
            tracing::info!("all required raft peers are connected: {required_peers:?}");
        }

        tracing::info!(
            "initializing raft membership (members_count={})",
            self.membership_nodes.len()
        );
        match self.raft.initialize(self.membership_nodes.clone()).await {
            Ok(()) => {
                tracing::info!("raft bootstrap process completed");
                Ok(())
            }
            Err(openraft::error::RaftError::APIError(
                openraft::error::InitializeError::NotAllowed(_),
            )) => {
                // Another node won the bootstrap race and initialized the cluster while
                // we were waiting for peers. This is a normal multi-node startup scenario
                // when multiple nodes have `bootstrap = true`. Safe to proceed.
                tracing::info!("raft cluster became initialized meanwhile; skipping bootstrap");
                Ok(())
            }
            Err(err) => Err(err.into()),
        }
    }
}
