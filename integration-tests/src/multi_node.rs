//! Test harness for multi-node consensus integration tests.
//!
//! [`ConsensusCluster`] models a cluster of consensus-enabled nodes that share an L1.
//! Construction allocates ports for every node up front and computes a full-mesh peer
//! list; lifecycle ops (`suspend`, `start`, `restart_with_overrides`, `remove`) work on
//! slot indices and preserve them across suspensions.
//!
//! Notable simplifications versus older revisions:
//! * Single `wait_healthy` based on the live (non-suspended) membership; it implicitly
//!   restarts any node whose runtime has panicked (typically the leadership-monitor
//!   panic on demotion) so a transient crash doesn't wedge the whole test.
//! * Strict, one-shot `send_transfer` — tests that want to tolerate leadership churn
//!   must wrap it themselves.
//! * The raft startup election gate (`lib/raft/src/init.rs`) eliminates the dominant
//!   stale-vote phantom-quorum case; the remaining panic paths are caught by the
//!   `wait_healthy` heal-and-poll loop.

use alloy::eips::BlockId;
use alloy::providers::Provider;
use anyhow::Context as _;
use futures::future::try_join_all;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;
use tokio::time::Instant;
use zksync_os_status_server::StatusResponse;

use crate::{
    AnvilL1, ChainLayout, Config, NodeRole, PROTOCOL_VERSION, Ports, StoppedTester, Tester,
    provider::ZksyncTestingProvider, test_config::disable_prover_input_generation,
};

const TEST_HEARTBEAT_INTERVAL: Duration = Duration::from_millis(100);
const TEST_ELECTION_TIMEOUT_MIN: Duration = Duration::from_secs(2);
const TEST_ELECTION_TIMEOUT_MAX: Duration = Duration::from_secs(4);
const NODE_STOP_TIMEOUT: Duration = Duration::from_secs(90);
const NODE_START_TIMEOUT: Duration = Duration::from_secs(180);
const HEALTH_POLL_INTERVAL: Duration = Duration::from_millis(200);

#[derive(Debug)]
enum NodeSlot {
    Running(Box<Tester>),
    Suspended(Box<StoppedTester>),
}

impl NodeSlot {
    fn running(&self) -> Option<&Tester> {
        match self {
            Self::Running(tester) => Some(tester),
            Self::Suspended(_) => None,
        }
    }
}

async fn with_node_lifecycle_timeout<T>(
    operation: &'static str,
    index: usize,
    timeout: Duration,
    future: impl Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    tokio::time::timeout(timeout, future)
        .await
        .with_context(|| format!("timed out {operation} node {index} after {timeout:?}"))?
        .with_context(|| format!("failed {operation} node {index}"))
}

/// Snapshot of the raft status of a chosen subset of nodes. Only used internally by
/// `wait_healthy`; not exposed to tests.
#[derive(Debug)]
struct ClusterState {
    nodes: Vec<(usize, Result<StatusResponse, String>)>,
}

impl ClusterState {
    async fn collect(cluster: &ConsensusCluster, indices: &[usize]) -> Self {
        let nodes = futures::future::join_all(indices.iter().copied().map(|idx| async move {
            let status = match cluster.nodes.get(idx) {
                Some(NodeSlot::Running(node)) => node.status().await.map_err(|e| e.to_string()),
                Some(NodeSlot::Suspended(_)) => Err("node is suspended".to_string()),
                None => Err("node index out of range".to_string()),
            };
            (idx, status)
        }))
        .await;
        Self { nodes }
    }

    fn status(&self, index: usize) -> Option<&StatusResponse> {
        self.nodes
            .iter()
            .find(|(idx, _)| *idx == index)
            .and_then(|(_, result)| result.as_ref().ok())
    }

    fn leader_indices(&self) -> Vec<usize> {
        self.nodes
            .iter()
            .filter_map(|(idx, result)| {
                result.as_ref().ok().and_then(|status| {
                    status
                        .consensus
                        .raft
                        .as_ref()
                        .filter(|r| r.is_leader)
                        .map(|_| *idx)
                })
            })
            .collect()
    }

    fn all_healthy(&self) -> bool {
        self.nodes
            .iter()
            .all(|(_, result)| matches!(result, Ok(status) if status.healthy))
    }

    fn all_have_leader(&self) -> bool {
        self.nodes
            .iter()
            .filter_map(|(_, result)| result.as_ref().ok())
            .all(|status| {
                status
                    .consensus
                    .raft
                    .as_ref()
                    .and_then(|r| r.current_leader.as_ref())
                    .is_some()
            })
    }

    fn agreed_leader(&self) -> Option<&str> {
        let leaders: Vec<_> = self
            .nodes
            .iter()
            .filter_map(|(_, result)| result.as_ref().ok())
            .filter_map(|status| status.consensus.raft.as_ref()?.current_leader.as_deref())
            .collect();

        leaders
            .first()
            .copied()
            .filter(|first| leaders.iter().all(|leader| leader == first))
    }

    /// Cluster is healthy when every observed node is healthy, exactly one of them claims to
    /// be leader, every node has a current_leader set, and all of them name the same one.
    fn formed_leader(&self) -> Option<usize> {
        let leader_indices = self.leader_indices();
        if leader_indices.len() != 1 {
            return None;
        }
        let leader_idx = leader_indices[0];
        let agreed = self.agreed_leader()?;
        let leader_node_id = self
            .status(leader_idx)
            .and_then(|s| s.consensus.raft.as_ref())
            .map(|r| r.node_id.as_str())?;
        if !self.all_healthy() || !self.all_have_leader() || agreed != leader_node_id {
            return None;
        }
        Some(leader_idx)
    }

    fn summary(&self) -> String {
        let leaders = self.leader_indices();
        format!(
            "healthy={} leaders={} all_have_leader={} agreed_leader={:?}",
            self.all_healthy(),
            leaders.len(),
            self.all_have_leader(),
            self.agreed_leader(),
        )
    }

    fn failure_reason(&self) -> String {
        let mut reasons = Vec::new();
        if !self.all_healthy() {
            let unhealthy: Vec<_> = self
                .nodes
                .iter()
                .filter_map(|(idx, result)| match result {
                    Ok(status) if !status.healthy => Some(format!("node_{idx}: healthy=false")),
                    Err(err) => Some(format!("node_{idx}: error={err:?}")),
                    _ => None,
                })
                .collect();
            reasons.push(format!("unhealthy: [{}]", unhealthy.join(", ")));
        }
        let leaders = self.leader_indices();
        if leaders.len() != 1 {
            reasons.push(format!("leader count = {} (expected 1)", leaders.len()));
        }
        if !self.all_have_leader() {
            reasons.push("some nodes have no current_leader".to_owned());
        }
        if self.agreed_leader().is_none() && self.all_have_leader() {
            reasons.push("nodes disagree on current_leader".to_owned());
        }
        if reasons.is_empty() {
            "unknown".to_owned()
        } else {
            reasons.join("; ")
        }
    }
}

/// Multi-node consensus test harness.
pub struct ConsensusCluster {
    nodes: Vec<NodeSlot>,
    batcher_node_index: usize,
}

impl ConsensusCluster {
    pub fn builder() -> ConsensusClusterBuilder {
        ConsensusClusterBuilder::default()
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn batcher_node_index(&self) -> usize {
        self.batcher_node_index
    }

    /// All slot indices, including suspended ones.
    pub fn indices(&self) -> Vec<usize> {
        (0..self.nodes.len()).collect()
    }

    /// Indices of currently running nodes.
    pub fn live_indices(&self) -> Vec<usize> {
        self.nodes
            .iter()
            .enumerate()
            .filter_map(|(idx, node)| node.running().is_some().then_some(idx))
            .collect()
    }

    pub fn is_suspended(&self, index: usize) -> bool {
        matches!(self.nodes[index], NodeSlot::Suspended(_))
    }

    /// Borrow a running node. Panics if the slot is suspended.
    pub fn node(&self, index: usize) -> &Tester {
        self.nodes[index]
            .running()
            .unwrap_or_else(|| panic!("node {index} is suspended"))
    }

    /// Gracefully shut down every running and suspended slot. Consumes the cluster.
    pub async fn shutdown_all(self) -> anyhow::Result<()> {
        for (index, node) in self.nodes.into_iter().enumerate() {
            match node {
                NodeSlot::Running(tester) => {
                    with_node_lifecycle_timeout(
                        "shutting down",
                        index,
                        NODE_STOP_TIMEOUT,
                        tester.shutdown(),
                    )
                    .await?
                }
                NodeSlot::Suspended(tester) => {
                    with_node_lifecycle_timeout(
                        "shutting down",
                        index,
                        NODE_STOP_TIMEOUT,
                        tester.shutdown(),
                    )
                    .await?
                }
            }
        }
        Ok(())
    }

    /// Stop a node's process while retaining its on-disk state and assigned ports so it
    /// can be restarted in the same slot via [`Self::start`].
    pub async fn suspend(&mut self, index: usize) -> anyhow::Result<()> {
        tracing::info!("suspending node {index}...");
        let slot = self.nodes.remove(index);
        let stopped = match slot {
            NodeSlot::Running(tester) => {
                with_node_lifecycle_timeout("suspending", index, NODE_STOP_TIMEOUT, tester.stop())
                    .await?
            }
            NodeSlot::Suspended(_) => panic!("node {index} is already suspended"),
        };
        self.nodes
            .insert(index, NodeSlot::Suspended(Box::new(stopped)));
        Ok(())
    }

    /// Restart a previously suspended node with its original config.
    pub async fn start(&mut self, index: usize) -> anyhow::Result<()> {
        self.start_inner(index, None::<fn(&mut Config)>).await
    }

    /// Restart a previously suspended node, applying additional overrides on top of its
    /// stored config. Used by tests that simulate operator-driven config changes such as
    /// toggling `force_clear_raft_history`.
    pub async fn restart_with_overrides(
        &mut self,
        index: usize,
        overrides: impl FnOnce(&mut Config),
    ) -> anyhow::Result<()> {
        self.start_inner(index, Some(overrides)).await
    }

    async fn start_inner(
        &mut self,
        index: usize,
        overrides: Option<impl FnOnce(&mut Config)>,
    ) -> anyhow::Result<()> {
        tracing::info!("starting suspended node {index}...");
        let slot = self.nodes.remove(index);
        let stopped = match slot {
            NodeSlot::Suspended(stopped) => stopped,
            NodeSlot::Running(_) => panic!("node {index} is not suspended"),
        };
        let started = match overrides {
            Some(f) => {
                with_node_lifecycle_timeout(
                    "starting with overrides",
                    index,
                    NODE_START_TIMEOUT,
                    stopped.start_with_overrides(f),
                )
                .await?
            }
            None => {
                with_node_lifecycle_timeout("starting", index, NODE_START_TIMEOUT, stopped.start())
                    .await?
            }
        };
        self.nodes
            .insert(index, NodeSlot::Running(Box::new(started)));
        Ok(())
    }

    /// Restart any running node whose runtime has reported a critical-task panic, reusing
    /// its on-disk state and ports. Mirrors what a production orchestrator does on a
    /// `reth_tasks` critical-task panic — notably the deliberate panic in
    /// `lib/raft/src/leadership_monitor.rs` when a leader is demoted mid-flight.
    ///
    /// Invoked implicitly from [`Self::wait_healthy`] on every poll. Storm tests that
    /// drive the cluster outside the wait-healthy loop (sending transfers in tight
    /// retry loops) can also call this directly to keep nodes alive between attempts.
    ///
    /// Returns the indices of nodes that were respawned in this sweep.
    pub async fn heal_crashed_nodes(&mut self) -> anyhow::Result<Vec<usize>> {
        let crashed: Vec<usize> = self
            .nodes
            .iter()
            .enumerate()
            .filter_map(|(idx, slot)| match slot {
                NodeSlot::Running(t) if t.has_crashed() => Some(idx),
                _ => None,
            })
            .collect();
        for &idx in &crashed {
            tracing::warn!("node {idx} crashed (critical task panicked); respawning...");
            let slot = self.nodes.remove(idx);
            let stopped = match slot {
                NodeSlot::Running(tester) => {
                    with_node_lifecycle_timeout(
                        "stopping crashed",
                        idx,
                        NODE_STOP_TIMEOUT,
                        tester.stop(),
                    )
                    .await?
                }
                NodeSlot::Suspended(_) => unreachable!("filtered to running above"),
            };
            let restarted = with_node_lifecycle_timeout(
                "restarting crashed",
                idx,
                NODE_START_TIMEOUT,
                stopped.start(),
            )
            .await?;
            self.nodes
                .insert(idx, NodeSlot::Running(Box::new(restarted)));
        }
        Ok(crashed)
    }

    /// Permanently shut down a node and drop its slot. Indices of nodes after `index`
    /// shift down by one.
    pub async fn remove(&mut self, index: usize) -> anyhow::Result<()> {
        tracing::info!("removing node {index}...");
        let slot = self.nodes.remove(index);
        match slot {
            NodeSlot::Running(tester) => {
                with_node_lifecycle_timeout(
                    "shutting down",
                    index,
                    NODE_STOP_TIMEOUT,
                    tester.shutdown(),
                )
                .await
            }
            NodeSlot::Suspended(stopped) => {
                with_node_lifecycle_timeout(
                    "shutting down",
                    index,
                    NODE_STOP_TIMEOUT,
                    stopped.shutdown(),
                )
                .await
            }
        }
    }

    /// Poll until every live node reports a single agreed leader and is healthy. Returns
    /// the leader's slot index.
    ///
    /// Each iteration first heals any node that has crashed (typically because its
    /// `leadership_monitor` panicked on leader demotion under load — see
    /// `lib/raft/src/leadership_monitor.rs`). This mirrors what a production orchestrator
    /// does on a critical-task panic and is the only practical way to keep test runs
    /// stable in CI without doing the bigger pipeline-cancellation refactor.
    pub async fn wait_healthy(&mut self, timeout: Duration) -> anyhow::Result<usize> {
        let deadline = Instant::now() + timeout;
        let mut last_summary = String::new();
        loop {
            self.heal_crashed_nodes().await?;
            let live = self.live_indices();
            anyhow::ensure!(!live.is_empty(), "no live nodes to wait on");
            let state = ClusterState::collect(self, &live).await;
            let summary = state.summary();
            if summary != last_summary {
                tracing::info!("cluster health (live={live:?}): {summary}");
                last_summary = summary;
            }
            if let Some(leader_idx) = state.formed_leader() {
                tracing::info!("cluster healthy (live={live:?}): leader_index={leader_idx}");
                return Ok(leader_idx);
            }
            if Instant::now() >= deadline {
                let final_state = ClusterState::collect(self, &live).await;
                anyhow::bail!(
                    "timed out waiting for cluster health among {live:?}: {}",
                    final_state.failure_reason()
                );
            }
            tokio::time::sleep(HEALTH_POLL_INTERVAL).await;
        }
    }

    /// Wait for every live node to expose `block_number` on its L2 RPC.
    pub async fn wait_replicated(
        &self,
        block_number: u64,
        timeout: Duration,
    ) -> anyhow::Result<()> {
        let waits = self
            .nodes
            .iter()
            .filter_map(NodeSlot::running)
            .map(|node| node.l2_zk_provider.wait_for_block(block_number));
        tokio::time::timeout(timeout, futures::future::try_join_all(waits))
            .await
            .with_context(|| {
                format!("timed out waiting for live nodes to reach L2 block {block_number}")
            })?
            .map(|_| ())
    }

    /// Wait for a specific node (must be live) to expose `block_number` on its L2 RPC.
    pub async fn wait_node_at(
        &self,
        index: usize,
        block_number: u64,
        timeout: Duration,
    ) -> anyhow::Result<()> {
        let node = self.node(index);
        tokio::time::timeout(timeout, node.l2_zk_provider.wait_for_block(block_number))
            .await
            .with_context(|| {
                format!("timed out waiting for node {index} to reach L2 block {block_number}")
            })?
            .map(|_| ())
    }

    /// Wait for L1 finalization of `block_number` via the batcher node, if it is currently
    /// running. Nop if the batcher slot is suspended.
    pub async fn wait_finalized_if_batcher_active(
        &self,
        block_number: u64,
        timeout: Duration,
    ) -> anyhow::Result<()> {
        let batcher_idx = self.batcher_node_index;
        if self.is_suspended(batcher_idx) {
            tracing::info!(
                block_number,
                batcher_idx,
                "skipping L1 finalization check (batcher node is suspended)"
            );
            return Ok(());
        }
        self.node(batcher_idx)
            .l2_zk_provider
            .wait_finalized_with_timeout(block_number, timeout)
            .await
            .with_context(|| {
                format!(
                    "block {block_number} was not finalized while batcher node {batcher_idx} was active"
                )
            })?;
        Ok(())
    }

    /// Latest L2 block exposed by the given node. Convenience for tests that compare
    /// heads before/after a state change.
    pub async fn latest_l2_block(&self, index: usize) -> anyhow::Result<u64> {
        const RPC_TIMEOUT: Duration = Duration::from_secs(10);
        tokio::time::timeout(
            RPC_TIMEOUT,
            self.node(index)
                .l2_zk_provider
                .get_block_number_by_id(BlockId::latest()),
        )
        .await
        .with_context(|| format!("timed out fetching latest L2 block from node {index}"))??
        .with_context(|| format!("node {index} did not return a latest block number"))
    }

    /// Fetch a node's raft node ID. Errors if the node is not exposing a raft status
    /// (e.g. consensus disabled).
    pub async fn raft_node_id(&self, index: usize) -> anyhow::Result<String> {
        self.node(index)
            .status()
            .await?
            .consensus
            .raft
            .map(|raft| raft.node_id)
            .with_context(|| format!("node {index} did not expose raft status"))
    }
}

#[derive(Default)]
pub struct ConsensusClusterBuilder {
    num_nodes: Option<usize>,
    batcher_node_index: Option<usize>,
}

impl ConsensusClusterBuilder {
    pub fn nodes(mut self, count: usize) -> Self {
        self.num_nodes = Some(count);
        self
    }

    /// Which slot runs the batcher. Defaults to 0. Exactly one node has
    /// `batcher_config.enabled = true`; the rest keep it disabled.
    pub fn batcher_node(mut self, index: usize) -> Self {
        self.batcher_node_index = Some(index);
        self
    }

    pub async fn build(self) -> anyhow::Result<ConsensusCluster> {
        let num_nodes = self.num_nodes.unwrap_or(1);
        assert!(num_nodes > 0, "ConsensusCluster needs at least 1 node");
        let batcher_node_index = self.batcher_node_index.unwrap_or(0);
        assert!(
            batcher_node_index < num_nodes,
            "batcher_node_index must be in 0..{num_nodes}"
        );

        // Pre-allocate all four ports per node so every other node can list the
        // network port in boot_nodes and the L2 RPC port in tx_forwarding_rpc_urls
        // before any node actually starts. This is the only construction-time
        // coupling between nodes; everything else is per-node.
        let mut node_ports = Vec::with_capacity(num_nodes);
        let mut secrets = Vec::with_capacity(num_nodes);
        for _ in 0..num_nodes {
            node_ports.push(Ports::acquire_unused().await?);
            secrets.push(zksync_os_network::rng_secret_key());
        }

        let node_records: Vec<_> = secrets
            .iter()
            .zip(&node_ports)
            .map(|(secret, ports)| {
                zksync_os_network::NodeRecord::from_secret_key(
                    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), ports.network.port),
                    secret,
                )
            })
            .collect();
        let peer_ids: Vec<_> = node_records.iter().map(|record| record.id).collect();
        let tx_forwarding_rpc_urls: Vec<_> = node_records
            .iter()
            .zip(&node_ports)
            .map(|(record, ports)| format!("{}@127.0.0.1:{}", record.id, ports.l2_rpc.port))
            .collect();

        let l1 = AnvilL1::start(ChainLayout::Default {
            protocol_version: PROTOCOL_VERSION,
        })
        .await?;

        let launches = secrets
            .into_iter()
            .zip(node_ports.into_iter())
            .enumerate()
            .map(|(i, (secret, ports))| {
                let peer_ids = peer_ids.clone();
                let tx_forwarding_rpc_urls = tx_forwarding_rpc_urls.clone();
                // Directed full mesh: lower-index nodes dial higher-index ones, so every pair has
                // exactly one maintained RLPx route. A symmetric all-trust-all setup causes
                // simultaneous crossed dials that devp2p drops as duplicates under stress; a ring
                // is too sparse for OpenRaft, which sends RPCs directly to every voter and needs a
                // path between every pair.
                let boot_nodes: Vec<zksync_os_network::TrustedPeer> = node_records
                    .iter()
                    .enumerate()
                    .filter_map(|(j, record)| (j > i).then_some((*record).into()))
                    .collect();
                let l1 = l1.clone();
                async move {
                    let network_port = ports.network.port;
                    let batcher_enabled = i == batcher_node_index;
                    tracing::info!(
                        "launching consensus node {i} (batcher_enabled={batcher_enabled}, port={network_port})"
                    );
                    let node = Tester::launch_node_with_ports(
                        l1,
                        false,
                        Some(move |config: &mut Config| {
                            config.general_config.node_role = NodeRole::MainNode;
                            config.general_config.main_node_rpc_url = None;
                            config.batcher_config.enabled = batcher_enabled;
                            // Consensus tests exercise raft/network behavior, not PIG.
                            // PIG creates CPU-bound background work that makes election
                            // timing flaky under suite stress.
                            disable_prover_input_generation(config);
                            config.network_config.enabled = true;
                            config.network_config.secret_key = Some(secret);
                            config.network_config.address = Ipv4Addr::LOCALHOST;
                            config.network_config.port = network_port;
                            config.network_config.boot_nodes = boot_nodes.clone();
                            config.consensus_config.enabled = true;
                            // Every node bootstraps. First initializer wins; others
                            // safely observe and skip (see `RaftBootstrapper`).
                            config.consensus_config.bootstrap = true;
                            config.consensus_config.peer_ids = peer_ids.clone();
                            config.consensus_config.tx_forwarding_rpc_urls =
                                tx_forwarding_rpc_urls.clone();
                            config.consensus_config.election_timeout_min =
                                TEST_ELECTION_TIMEOUT_MIN;
                            config.consensus_config.election_timeout_max =
                                TEST_ELECTION_TIMEOUT_MAX;
                            config.consensus_config.heartbeat_interval = TEST_HEARTBEAT_INTERVAL;
                        }),
                        ChainLayout::Default {
                            protocol_version: PROTOCOL_VERSION,
                        },
                        ports,
                        false,
                    )
                    .await?;
                    anyhow::Ok(NodeSlot::Running(Box::new(node)))
                }
            });

        Ok(ConsensusCluster {
            nodes: try_join_all(launches).await?,
            batcher_node_index,
        })
    }
}
