use crate::config::NetworkConfig;
use crate::protocol::{
    ConnectionRegistry, ExternalNodeProtocolConfig, HandlerSharedState, MainNodeProtocolConfig,
    ProtocolEvent, ZksProtocolConfig, ZksProtocolHandler,
};
use crate::session::PeerSessionStore;
use crate::version::{ZksProtocolV1, ZksProtocolV2, ZksProtocolV3};
use crate::wire::message::ZksMessage;
use crate::{VerifyBatch, VerifyBatchResult};
use alloy::eips::eip2124::Head;
use backon::{ConstantBuilder, Retryable};
use futures::future::join_all;
use reth_chainspec::{ChainSpecProvider, EthChainSpec, Hardforks};
use reth_discv5::discv5;
use reth_eth_wire::HelloMessageWithProtocols;
use reth_net_nat::NatResolver;
use reth_network::error::NetworkError;
use reth_network::types::peers::config::PeerBackoffDurations;
use reth_network::{
    NetworkConfig as RethNetworkConfig, NetworkConfigBuilder, NetworkManager, PeersConfig,
};
use reth_network_peers::PeerId;
use reth_network_peers::{NodeRecord, TrustedPeer};
use reth_provider::BlockNumReader;
use reth_tasks::Runtime;
use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::net::{SocketAddr, SocketAddrV4};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use zksync_os_metadata::NODE_CLIENT_VERSION;
use zksync_os_storage_api::ReadReplay;

/// Max number of active devp2p connections.
const MAX_ACTIVE_CONNECTIONS: usize = 25;
/// Retry boot node DNS resolution for up to ~2 minutes so discv5 bootstrap has usable peers.
const BOOT_NODE_RESOLUTION_RETRY_DELAY: Duration = Duration::from_secs(5);
const BOOT_NODE_RESOLUTION_MAX_RETRIES: usize = 24;
const BOOT_NODE_RESOLUTION_RETRY_BUILDER: ConstantBuilder = ConstantBuilder::new()
    .with_delay(BOOT_NODE_RESOLUTION_RETRY_DELAY)
    .with_max_times(BOOT_NODE_RESOLUTION_MAX_RETRIES);

#[derive(Debug, thiserror::Error)]
#[error("failed to resolve any configured boot nodes before starting the p2p network")]
struct BootNodeResolutionError {
    unresolved_boot_nodes: usize,
}

#[derive(Debug)]
struct BootNodeResolutionState {
    unresolved_boot_nodes: Vec<TrustedPeer>,
    resolved_boot_nodes: Vec<TrustedPeer>,
}

async fn resolve_boot_nodes_with_retry(
    boot_nodes: Vec<TrustedPeer>,
) -> Result<Vec<TrustedPeer>, NetworkError> {
    if boot_nodes.is_empty() {
        return Ok(vec![]);
    }

    let state = Arc::new(Mutex::new(BootNodeResolutionState {
        resolved_boot_nodes: Vec::with_capacity(boot_nodes.len()),
        unresolved_boot_nodes: boot_nodes,
    }));
    let resolve = Arc::new(|boot_node: TrustedPeer| async move { boot_node.resolve().await });

    {
        let state = Arc::clone(&state);
        let resolve = Arc::clone(&resolve);
        move || {
            let state = Arc::clone(&state);
            let resolve = Arc::clone(&resolve);
            async move { resolve_boot_nodes_once(&state, resolve.as_ref()).await }
        }
    }
    .retry(BOOT_NODE_RESOLUTION_RETRY_BUILDER)
    .notify(|error, retry_in| {
        tracing::info!(
            retry_in = ?retry_in,
            unresolved_boot_nodes = error.unresolved_boot_nodes,
            "retrying boot node resolution before starting p2p network"
        );
    })
    .await
    .map_err(|error| io::Error::new(io::ErrorKind::AddrNotAvailable, error))?;

    let state = state.lock().expect("boot node resolution state poisoned");
    if !state.unresolved_boot_nodes.is_empty() {
        tracing::warn!(
            resolved_boot_nodes = state.resolved_boot_nodes.len(),
            unresolved_boot_nodes = state.unresolved_boot_nodes.len(),
            "starting p2p network with partially resolved boot nodes"
        );
    }
    Ok(state.resolved_boot_nodes.clone())
}

async fn resolve_boot_nodes_once<Resolve, ResolveFut>(
    state: &Arc<Mutex<BootNodeResolutionState>>,
    resolve: &Resolve,
) -> Result<(), BootNodeResolutionError>
where
    Resolve: Fn(TrustedPeer) -> ResolveFut,
    ResolveFut: Future<Output = io::Result<NodeRecord>>,
{
    let unresolved_boot_nodes = {
        state
            .lock()
            .expect("boot node resolution state poisoned")
            .unresolved_boot_nodes
            .clone()
    };
    let resolution_results = join_all(unresolved_boot_nodes.into_iter().map(|boot_node| {
        let resolution = resolve(boot_node.clone());
        async move { (boot_node, resolution.await) }
    }))
    .await;

    let mut state = state.lock().expect("boot node resolution state poisoned");
    state.unresolved_boot_nodes.clear();
    for (boot_node, resolution) in resolution_results {
        match resolution {
            Ok(record) => {
                tracing::info!(boot_node = %boot_node, resolved = ?record, "resolved boot node");
                state.resolved_boot_nodes.push(record.into());
            }
            Err(err) => {
                tracing::warn!(boot_node = %boot_node, %err, "failed to resolve boot node");
                state.unresolved_boot_nodes.push(boot_node);
            }
        }
    }

    if state.unresolved_boot_nodes.is_empty() || !state.resolved_boot_nodes.is_empty() {
        Ok(())
    } else {
        Err(BootNodeResolutionError {
            unresolved_boot_nodes: state.unresolved_boot_nodes.len(),
        })
    }
}

/// Manages the entire network state including all RLPx subprotocols and discv5 peer discovery.
///
/// This type is supposed to be consumed through [`NetworkService::spawn`] that registers it as an
/// endless task that consistently drives the state of the entire network forward.
#[derive(Debug)]
pub struct NetworkService {
    network_manager: NetworkManager,
    protocol_rx: mpsc::UnboundedReceiver<ProtocolEvent>,
    peer_sessions: Arc<RwLock<PeerSessionStore>>,
    connection_registry: ConnectionRegistry,
}

#[derive(Debug, Clone)]
pub struct PeerVerifyBatch {
    pub peer_id: PeerId,
    pub message: VerifyBatch,
}

#[derive(Debug, Clone)]
pub struct PeerVerifyBatchResult {
    pub peer_id: PeerId,
    pub message: VerifyBatchResult,
}

impl NetworkService {
    pub async fn new(
        config: NetworkConfig,
        protocol_config: ZksProtocolConfig,
        replay: impl ReadReplay + Clone,
        client: impl ChainSpecProvider<ChainSpec: Hardforks> + BlockNumReader + 'static,
    ) -> Result<Self, NetworkError> {
        // Install ViseRecorder before creating the NetworkManager so that reth-network metrics
        // are captured. This must happen before `NetworkManager::builder()` because that is where
        // reth initializes its metric handles (via `Default::default()` on each metrics struct).
        crate::metrics::install_recorder();
        match NatResolver::Any.external_addr().await {
            None => {
                tracing::info!("could not resolve external IP (STUN)");
            }
            Some(ip) => {
                tracing::info!(%ip, "resolved external IP (STUN)");
            }
        };
        let rlpx_address = SocketAddr::V4(SocketAddrV4::new(config.address, config.port));
        let chain_spec = client.chain_spec();
        let genesis = Head {
            hash: chain_spec.genesis_hash(),
            number: 0,
            timestamp: chain_spec.genesis().timestamp,
            difficulty: chain_spec.genesis().difficulty,
            total_difficulty: chain_spec.genesis().difficulty,
        };
        let fork_id = chain_spec.fork_id(&genesis);
        let boot_nodes = resolve_boot_nodes_with_retry(config.boot_nodes.clone()).await?;
        tracing::info!(?genesis, ?fork_id, "initializing p2p network service");
        let (protocol_tx, protocol_rx) = mpsc::unbounded_channel();
        let cfg_builder = RethNetworkConfig::builder(config.secret_key)
            .boot_nodes(boot_nodes)
            // Configure node identity
            .apply(|builder| {
                let peer_id = builder.get_peer_id();
                builder.hello_message(
                    HelloMessageWithProtocols::builder(peer_id)
                        .client_version(NODE_CLIENT_VERSION)
                        .build(),
                )
            })
            // Disable Node Discovery Protocol v4 as ZKsync OS only uses v5
            .disable_discv4_discovery()
            // Disable DNS-based discovery (EIP-1459), unused in ZKsync OS
            .disable_dns_discovery()
            // Disable built-in NAT resolver as discv5 does not need it (ENR socket address is
            // updated based on PONG responses from the majority of peers)
            .disable_nat()
            // Setup Node Discovery Protocol v5 on `localhost:<port>:UDP` that points to RLPx socket
            // at `localhost:<port>:TCP`
            .discovery_v5(
                reth_discv5::Config::builder(rlpx_address)
                    .discv5_config(
                        discv5::ConfigBuilder::new(discv5::ListenConfig::from_ip(
                            rlpx_address.ip(),
                            config.port,
                        ))
                        // Require only 2 peers to agree on our external IP to update our local ENR
                        .enr_peer_update_min(2)
                        // 2 peers from above must agree on external IP within 1h from each other.
                        // This can make the node less responsive to dynamic IP changes.
                        .vote_duration(Duration::from_secs(3600))
                        // Sets peer ban duration to 1 second, effectively disabling it
                        .ban_duration(Some(Duration::from_secs(1)))
                        .build(),
                    )
                    // Specify custom fork id configuration
                    .fork(b"zksync-os", fork_id),
            )
            .peer_config(
                PeersConfig::default()
                    // Sets peer ban duration to 1 second, effectively disabling it
                    .with_ban_duration(Duration::from_secs(1))
                    // Tune backoff durations to be low, useful while we are in exploratory phase
                    // and infra issues are expected.
                    .with_backoff_durations(PeerBackoffDurations {
                        low: Duration::from_secs(30),
                        medium: Duration::from_secs(60),
                        high: Duration::from_secs(60 * 2),
                        max: Duration::from_secs(60 * 3),
                    })
                    // Peers' fork id must match, otherwise we could discover peers from other
                    // chains.
                    .with_enforce_enr_fork_id(true),
            )
            // Use the same port for RLPx (TCP) and for discv5 (UDP)
            .listener_addr(rlpx_address)
            .discovery_addr(rlpx_address)
            // Disable transaction gossip as it is unsupported by ZKsync OS
            .disable_tx_gossip(true)
            // Do not require any block hashes in `eth` RLPx protocol as it is unused
            .required_block_hashes(vec![])
            // Set network id to ZKsync OS chain's id, otherwise we might connect to unrelated peers
            .network_id(Some(chain_spec.chain_id()))
            // Use genesis as chain head
            .set_head(genesis);
        let connection_registry: ConnectionRegistry = Arc::new(RwLock::new(HashMap::new()));
        let cfg_builder = match protocol_config {
            ZksProtocolConfig::MainNode(protocol) => Self::register_main_node_rlpx_sub_protocols(
                cfg_builder,
                protocol,
                replay,
                protocol_tx,
                connection_registry.clone(),
            ),
            ZksProtocolConfig::ExternalNode(protocol) => {
                Self::register_external_node_rlpx_sub_protocols(
                    cfg_builder,
                    protocol,
                    replay,
                    protocol_tx,
                    connection_registry.clone(),
                )
            }
        };
        let net_cfg = cfg_builder.build(client);
        tracing::debug!(?net_cfg, "starting p2p network service");
        // Create network manager. We are not interested in `txpool` because transaction gossip is
        // disabled. `request_handler` is also unused as it is specific to `eth` protocol.
        let (network_manager, _txpool, _request_handler) =
            NetworkManager::builder(net_cfg).await?.split();

        Ok(Self {
            network_manager,
            protocol_rx,
            peer_sessions: Arc::new(RwLock::new(PeerSessionStore::default())),
            connection_registry,
        })
    }

    fn register_main_node_rlpx_sub_protocols(
        builder: NetworkConfigBuilder,
        protocol: MainNodeProtocolConfig,
        replay: impl ReadReplay + Clone,
        protocol_tx: mpsc::UnboundedSender<ProtocolEvent>,
        connection_registry: ConnectionRegistry,
    ) -> NetworkConfigBuilder {
        let state = HandlerSharedState::new(protocol_tx, MAX_ACTIVE_CONNECTIONS);
        builder
            // Support for v1 must be dropped before upgrade to protocol version v31.0. Otherwise,
            // we might send invalid record to ENs that are still using v1 protocol (`starting_migration_number`
            // in those record is always 0).
            .add_rlpx_sub_protocol(ZksProtocolHandler::<ZksProtocolV1, _>::for_main_node(
                replay.clone(),
                protocol.clone(),
                state.clone(),
                connection_registry.clone(),
            ))
            .add_rlpx_sub_protocol(ZksProtocolHandler::<ZksProtocolV2, _>::for_main_node(
                replay.clone(),
                protocol.clone(),
                state.clone(),
                connection_registry.clone(),
            ))
            .add_rlpx_sub_protocol(ZksProtocolHandler::<ZksProtocolV3, _>::for_main_node(
                replay,
                protocol,
                state,
                connection_registry,
            ))
    }

    fn register_external_node_rlpx_sub_protocols(
        builder: NetworkConfigBuilder,
        protocol: ExternalNodeProtocolConfig,
        replay: impl ReadReplay + Clone,
        protocol_tx: mpsc::UnboundedSender<ProtocolEvent>,
        connection_registry: ConnectionRegistry,
    ) -> NetworkConfigBuilder {
        let state = HandlerSharedState::new(protocol_tx, MAX_ACTIVE_CONNECTIONS);
        builder
            .add_rlpx_sub_protocol(ZksProtocolHandler::<ZksProtocolV1, _>::for_external_node(
                replay.clone(),
                protocol.clone(),
                state.clone(),
                connection_registry.clone(),
            ))
            .add_rlpx_sub_protocol(ZksProtocolHandler::<ZksProtocolV2, _>::for_external_node(
                replay.clone(),
                protocol.clone(),
                state.clone(),
                connection_registry.clone(),
            ))
            .add_rlpx_sub_protocol(ZksProtocolHandler::<ZksProtocolV3, _>::for_external_node(
                replay,
                protocol,
                state,
                connection_registry,
            ))
    }

    /// Consume the service by registering it as the set of long-running tasks that drive p2p
    /// networking forward.
    ///
    /// When `verify_request_rx` is provided, an additional main-node-only dispatcher task is
    /// spawned to forward outgoing `VerifyBatch` requests to eligible peers. Passing `None`
    /// disables that dispatcher while keeping the core network and protocol event tasks running.
    pub fn spawn(
        mut self,
        runtime: &Runtime,
        verify_request_rx: Option<mpsc::Receiver<VerifyBatch>>,
    ) {
        let peer_sessions = Arc::clone(&self.peer_sessions);
        let connection_registry = Arc::clone(&self.connection_registry);
        if let Some(mut verify_request_rx) = verify_request_rx {
            runtime.spawn_critical_task("p2p verify dispatcher", async move {
                while let Some(request) = verify_request_rx.recv().await {
                    dispatch_verify_batch(&peer_sessions, &connection_registry, request).await;
                }
            });
        }
        runtime.spawn_critical_with_graceful_shutdown_signal(
            "p2p network task",
            |shutdown| async move {
                self.network_manager
                    .run_until_graceful_shutdown(shutdown, |_network| {
                        // todo: save peers to disk like reth?
                    })
                    .await;
                tracing::info!("p2p network graceful shutdown complete");
            },
        );
        runtime.spawn_critical_task("p2p session tracker", async move {
            while let Some(event) = self.protocol_rx.recv().await {
                let now = Instant::now();
                let mut peer_sessions = self.peer_sessions.write().unwrap();
                match event {
                    ProtocolEvent::Established {
                        peer_id,
                        remote_addr,
                        ..
                    } => {
                        peer_sessions.insert(now, peer_id, remote_addr);
                        tracing::debug!(
                            peer_id = %peer_id,
                            session = ?peer_sessions.get(peer_id),
                            "peer connected"
                        );
                    }
                    ProtocolEvent::Closed { peer_id } => {
                        let removed = peer_sessions.remove(peer_id);
                        tracing::debug!(
                            peer_id = %peer_id,
                            session = ?removed,
                            "peer session closed"
                        );
                    }
                    ProtocolEvent::ReplayRequested {
                        peer_id,
                        starting_block,
                    } => {
                        peer_sessions.replay_requested(peer_id, starting_block);
                        tracing::debug!(
                            peer_id = %peer_id,
                            session = ?peer_sessions.get(peer_id),
                            "peer replay requested"
                        );
                    }
                    ProtocolEvent::VerifierRoleRequested { peer_id } => {
                        peer_sessions.verifier_role_requested(peer_id);
                        tracing::debug!(
                            peer_id = %peer_id,
                            session = ?peer_sessions.get(peer_id),
                            "peer verifier role requested"
                        );
                    }
                    ProtocolEvent::VerifierChallengeSent { peer_id, nonce } => {
                        peer_sessions.verifier_challenged(peer_id, nonce);
                        tracing::debug!(
                            peer_id = %peer_id,
                            session = ?peer_sessions.get(peer_id),
                            "peer verifier challenge sent"
                        );
                    }
                    ProtocolEvent::VerifierAuthorized { peer_id, signer } => {
                        peer_sessions.verifier_authorized(peer_id, signer);
                        tracing::debug!(
                            peer_id = %peer_id,
                            session = ?peer_sessions.get(peer_id),
                            "peer verifier authorized"
                        );
                    }
                    ProtocolEvent::VerifierUnauthorized { peer_id, signer } => {
                        peer_sessions.verifier_unauthorized(peer_id, signer);
                        tracing::debug!(
                            peer_id = %peer_id,
                            session = ?peer_sessions.get(peer_id),
                            "peer verifier unauthorized"
                        );
                    }
                    ProtocolEvent::ReplayBlockSent {
                        peer_id,
                        block_number,
                    } => {
                        peer_sessions.replay_block_sent(now, peer_id, block_number);
                        tracing::debug!(
                            peer_id = %peer_id,
                            session = ?peer_sessions.get(peer_id),
                            "peer replay progress updated"
                        );
                    }
                    ProtocolEvent::MaxActiveConnectionsExceeded { max_connections } => {
                        tracing::warn!(max_connections, "max active connections exceeded");
                    }
                }
            }
        });
    }
}

/// Dispatches a verify request to all currently eligible verifier peers.
///
/// Eligibility is derived from [`PeerSessionStore`], then cross-checked against the live
/// [`ConnectionRegistry`] before sending. Only `zks/3` peers can receive `VerifyBatch`.
async fn dispatch_verify_batch(
    peer_sessions: &Arc<RwLock<PeerSessionStore>>,
    connection_registry: &ConnectionRegistry,
    request: VerifyBatch,
) {
    let required_block = request.last_block_number;
    let eligible_peers: Vec<_> = {
        let peer_sessions = peer_sessions.read().unwrap();
        peer_sessions
            .authorized_verifier_peers(required_block)
            .collect()
    };

    if eligible_peers.is_empty() {
        tracing::warn!(
            request_id = request.request_id,
            batch_number = request.batch_number,
            required_block,
            "skipping verify request: no eligible verifier peers"
        );
        return;
    }

    let dispatch_targets: Vec<_> = {
        let connection_registry = connection_registry.read().unwrap();
        eligible_peers
            .into_iter()
            .map(|peer_id| (peer_id, connection_registry.get(&peer_id).cloned()))
            .collect()
    };
    let mut sent = 0usize;
    for (peer_id, connection) in dispatch_targets {
        let Some(connection) = connection else {
            tracing::warn!(
                peer_id = %peer_id,
                request_id = request.request_id,
                batch_number = request.batch_number,
                "skipping verify request: missing active connection"
            );
            continue;
        };
        if connection.version < crate::version::ZksVersion::Zks3 {
            tracing::warn!(
                peer_id = %peer_id,
                request_id = request.request_id,
                batch_number = request.batch_number,
                version = ?connection.version,
                "skipping verify request: peer is not on zks/3"
            );
            continue;
        }
        if connection
            .outbound_tx
            .send(ZksMessage::<ZksProtocolV3>::VerifyBatch(request.clone()).encoded())
            .await
            .is_err()
        {
            tracing::warn!(
                peer_id = %peer_id,
                request_id = request.request_id,
                batch_number = request.batch_number,
                "failed to dispatch verify request"
            );
            continue;
        }
        sent += 1;
        tracing::info!(
            peer_id = %peer_id,
            request_id = request.request_id,
            batch_number = request.batch_number,
            required_block,
            "dispatched verify request"
        );
    }

    tracing::info!(
        request_id = request.request_id,
        batch_number = request.batch_number,
        required_block,
        sent,
        "finished verify request dispatch"
    );
}

#[cfg(test)]
mod tests {
    use super::BOOT_NODE_RESOLUTION_MAX_RETRIES;
    use super::BOOT_NODE_RESOLUTION_RETRY_BUILDER;
    use super::BOOT_NODE_RESOLUTION_RETRY_DELAY;
    use super::BootNodeResolutionState;
    use super::ConnectionRegistry;
    use super::dispatch_verify_batch;
    use super::resolve_boot_nodes_once;
    use crate::VerifyBatch;
    use crate::protocol::PeerConnectionHandle;
    use crate::session::PeerSessionStore;
    use crate::version::{ZksProtocolV3, ZksVersion};
    use crate::wire::message::ZksMessage;
    use alloy::primitives::{Address, B512, Bytes};
    use backon::{Retryable, Sleeper};
    use reth_network::error::NetworkError;
    use reth_network_peers::PeerId;
    use reth_network_peers::{NodeRecord, TrustedPeer};
    use std::collections::{HashMap, VecDeque};
    use std::future::Future;
    use std::io;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::sync::{Arc, Mutex, RwLock};
    use std::time::Instant;
    use tokio::sync::mpsc;

    const NODE_A: &str = "enode://6f8a80d14311c39f35f516fa664deaaaa13e85b2f7493f37f6144d86991ec012937307647bd3b9a82abe2974e1407241d54947bbb39763a4cac9f77166ad92a0@node-a.internal:30303?discport=30301";
    const NODE_B: &str = "enode://1dd9d65c4552b5eb43d5ad55a2ee3f56c6cbc1c64a5c8d659f51fcd51bace24351232b8d7821617d2b29b54b81cdefb9b3e9c37d7fd5f63270bcc9e1a6f6a439@node-b.internal:30303?discport=30301";
    const NODE_A_IP: &str = "enode://6f8a80d14311c39f35f516fa664deaaaa13e85b2f7493f37f6144d86991ec012937307647bd3b9a82abe2974e1407241d54947bbb39763a4cac9f77166ad92a0@10.0.0.10:30303?discport=30301";
    const NODE_B_IP: &str = "enode://1dd9d65c4552b5eb43d5ad55a2ee3f56c6cbc1c64a5c8d659f51fcd51bace24351232b8d7821617d2b29b54b81cdefb9b3e9c37d7fd5f63270bcc9e1a6f6a439@10.0.0.11:30303?discport=30301";

    fn trusted_peer(enode: &str) -> TrustedPeer {
        enode.parse().unwrap()
    }

    fn node_record(enode: &str) -> NodeRecord {
        trusted_peer(enode).resolve_blocking().unwrap()
    }

    async fn resolve_boot_nodes_with_retry_using<Resolve, ResolveFut, Sleep>(
        boot_nodes: Vec<TrustedPeer>,
        resolve: Resolve,
        sleep: Sleep,
    ) -> Result<Vec<TrustedPeer>, NetworkError>
    where
        Resolve: Fn(TrustedPeer) -> ResolveFut + 'static,
        ResolveFut: Future<Output = io::Result<NodeRecord>>,
        Sleep: Sleeper,
    {
        if boot_nodes.is_empty() {
            return Ok(vec![]);
        }

        let state = Arc::new(Mutex::new(BootNodeResolutionState {
            resolved_boot_nodes: Vec::with_capacity(boot_nodes.len()),
            unresolved_boot_nodes: boot_nodes,
        }));
        let resolve = Arc::new(resolve);

        {
            let state = Arc::clone(&state);
            let resolve = Arc::clone(&resolve);
            move || {
                let state = Arc::clone(&state);
                let resolve = Arc::clone(&resolve);
                async move { resolve_boot_nodes_once(&state, resolve.as_ref()).await }
            }
        }
        .retry(BOOT_NODE_RESOLUTION_RETRY_BUILDER)
        .sleep(sleep)
        .await
        .map_err(|error| io::Error::new(io::ErrorKind::AddrNotAvailable, error))?;

        Ok(state
            .lock()
            .expect("boot node resolution state poisoned")
            .resolved_boot_nodes
            .clone())
    }

    #[test_log::test(tokio::test(flavor = "current_thread"))]
    async fn boot_node_resolution_retries_until_any_boot_node_resolves() {
        let responses = Arc::new(Mutex::new(HashMap::from([
            (
                NODE_A.to_owned(),
                VecDeque::from([None, Some(node_record(NODE_A_IP))]),
            ),
            (NODE_B.to_owned(), VecDeque::from([None, None])),
        ])));
        let sleeps = Arc::new(Mutex::new(Vec::new()));

        let resolved = resolve_boot_nodes_with_retry_using(
            vec![trusted_peer(NODE_A), trusted_peer(NODE_B)],
            {
                let responses = Arc::clone(&responses);
                move |boot_node| {
                    let responses = Arc::clone(&responses);
                    async move {
                        let mut responses = responses.lock().unwrap();
                        let queue = responses
                            .get_mut(&boot_node.to_string())
                            .expect("missing resolver response queue");
                        match queue.pop_front().expect("resolver queue exhausted") {
                            Some(record) => Ok(record),
                            None => Err(io::Error::new(
                                io::ErrorKind::AddrNotAvailable,
                                "dns not ready",
                            )),
                        }
                    }
                }
            },
            {
                let sleeps = Arc::clone(&sleeps);
                move |duration| {
                    let sleeps = Arc::clone(&sleeps);
                    async move {
                        sleeps.lock().unwrap().push(duration);
                    }
                }
            },
        )
        .await
        .unwrap();

        assert_eq!(resolved, vec![trusted_peer(NODE_A_IP)]);
        assert_eq!(
            &*sleeps.lock().unwrap(),
            &[BOOT_NODE_RESOLUTION_RETRY_DELAY]
        );
    }

    #[test_log::test(tokio::test(flavor = "current_thread"))]
    async fn boot_node_resolution_uses_configured_retry_budget() {
        let attempts = BOOT_NODE_RESOLUTION_MAX_RETRIES + 1;
        let responses = Arc::new(Mutex::new(HashMap::from([(
            NODE_A.to_owned(),
            std::iter::repeat_n(None, attempts).collect::<VecDeque<_>>(),
        )])));
        let sleeps = Arc::new(Mutex::new(Vec::new()));

        let err = resolve_boot_nodes_with_retry_using(
            vec![trusted_peer(NODE_A)],
            {
                let responses = Arc::clone(&responses);
                move |boot_node| {
                    let responses = Arc::clone(&responses);
                    async move {
                        let mut responses = responses.lock().unwrap();
                        let queue = responses
                            .get_mut(&boot_node.to_string())
                            .expect("missing resolver response queue");
                        match queue.pop_front().expect("resolver queue exhausted") {
                            Some(record) => Ok(record),
                            None => Err(io::Error::new(
                                io::ErrorKind::AddrNotAvailable,
                                "dns not ready",
                            )),
                        }
                    }
                }
            },
            {
                let sleeps = Arc::clone(&sleeps);
                move |duration| {
                    let sleeps = Arc::clone(&sleeps);
                    async move {
                        sleeps.lock().unwrap().push(duration);
                    }
                }
            },
        )
        .await
        .unwrap_err();

        match err {
            NetworkError::Io(err) => assert_eq!(err.kind(), io::ErrorKind::AddrNotAvailable),
            other => panic!("unexpected error: {other:?}"),
        }
        assert_eq!(
            sleeps.lock().unwrap().len(),
            BOOT_NODE_RESOLUTION_MAX_RETRIES
        );
        assert!(
            sleeps
                .lock()
                .unwrap()
                .iter()
                .all(|delay| *delay == BOOT_NODE_RESOLUTION_RETRY_DELAY)
        );
    }

    #[test_log::test(tokio::test(flavor = "current_thread"))]
    async fn boot_node_resolution_returns_immediately_when_all_nodes_resolve() {
        let sleeps = Arc::new(Mutex::new(Vec::new()));

        let resolved = resolve_boot_nodes_with_retry_using(
            vec![trusted_peer(NODE_A), trusted_peer(NODE_B)],
            |boot_node| {
                let record = match boot_node.to_string().as_str() {
                    NODE_A => node_record(NODE_A_IP),
                    NODE_B => node_record(NODE_B_IP),
                    _ => panic!("unexpected boot node"),
                };
                async move { Ok(record) }
            },
            {
                let sleeps = Arc::clone(&sleeps);
                move |duration| {
                    let sleeps = Arc::clone(&sleeps);
                    async move {
                        sleeps.lock().unwrap().push(duration);
                    }
                }
            },
        )
        .await
        .unwrap();

        assert_eq!(
            resolved,
            vec![trusted_peer(NODE_A_IP), trusted_peer(NODE_B_IP)]
        );
        assert!(sleeps.lock().unwrap().is_empty());
    }

    fn peer_id(byte: u8) -> PeerId {
        B512::repeat_byte(byte)
    }

    fn socket_addr(port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
    }

    fn verify_request() -> VerifyBatch {
        VerifyBatch {
            request_id: 41,
            batch_number: 7,
            first_block_number: 100,
            last_block_number: 120,
            pubdata_mode: 0,
            commit_data: Bytes::from_static(b"commit"),
            prev_commit_data: Bytes::from_static(b"prev"),
            execution_protocol_version: 31,
        }
    }

    fn add_authorized_peer(
        store: &mut PeerSessionStore,
        peer_id: PeerId,
        last_block_sent: u64,
        signer: Address,
    ) {
        let now = Instant::now();
        store.insert(now, peer_id, socket_addr(30_300 + u16::from(peer_id[0])));
        store.replay_requested(peer_id, 1);
        store.replay_block_sent(now, peer_id, last_block_sent);
        store.verifier_authorized(peer_id, signer);
    }

    #[test_log::test(tokio::test(flavor = "current_thread"))]
    async fn dispatch_verify_batch_sends_only_to_authorized_caught_up_zks3_peers() {
        let eligible_zks3_peer = peer_id(0x11);
        let lagging_zks3_peer = peer_id(0x22);
        let zks2_peer = peer_id(0x33);
        let unauthorized_peer = peer_id(0x44);
        let missing_connection_peer = peer_id(0x55);
        let signer = Address::repeat_byte(0xAA);

        let mut store = PeerSessionStore::default();
        add_authorized_peer(&mut store, eligible_zks3_peer, 120, signer);
        add_authorized_peer(&mut store, lagging_zks3_peer, 119, signer);
        add_authorized_peer(&mut store, zks2_peer, 120, signer);
        add_authorized_peer(&mut store, missing_connection_peer, 120, signer);
        let now = Instant::now();
        store.insert(now, unauthorized_peer, socket_addr(30_368));
        store.replay_requested(unauthorized_peer, 1);
        store.replay_block_sent(now, unauthorized_peer, 120);
        store.verifier_unauthorized(unauthorized_peer, Some(signer));

        let peer_sessions = Arc::new(RwLock::new(store));
        let connection_registry: ConnectionRegistry = Arc::new(RwLock::new(HashMap::new()));

        let (eligible_tx, mut eligible_rx) = mpsc::channel(1);
        let (lagging_tx, mut lagging_rx) = mpsc::channel(1);
        let (zks2_tx, mut zks2_rx) = mpsc::channel(1);
        let (unauthorized_tx, mut unauthorized_rx) = mpsc::channel(1);

        {
            let mut registry = connection_registry.write().unwrap();
            registry.insert(
                eligible_zks3_peer,
                PeerConnectionHandle {
                    version: ZksVersion::Zks3,
                    outbound_tx: eligible_tx,
                },
            );
            registry.insert(
                lagging_zks3_peer,
                PeerConnectionHandle {
                    version: ZksVersion::Zks3,
                    outbound_tx: lagging_tx,
                },
            );
            registry.insert(
                zks2_peer,
                PeerConnectionHandle {
                    version: ZksVersion::Zks2,
                    outbound_tx: zks2_tx,
                },
            );
            registry.insert(
                unauthorized_peer,
                PeerConnectionHandle {
                    version: ZksVersion::Zks3,
                    outbound_tx: unauthorized_tx,
                },
            );
        }

        let request = verify_request();
        dispatch_verify_batch(&peer_sessions, &connection_registry, request.clone()).await;

        let encoded =
            tokio::time::timeout(std::time::Duration::from_millis(250), eligible_rx.recv())
                .await
                .expect("eligible zks/3 peer should receive verify request")
                .expect("eligible zks/3 peer channel closed");
        let mut slice = encoded.as_ref();
        let decoded = ZksMessage::<ZksProtocolV3>::decode_message(&mut slice).unwrap();
        match decoded {
            ZksMessage::VerifyBatch(actual) => assert_eq!(actual, request),
            other => panic!("unexpected message dispatched: {other:?}"),
        }

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), lagging_rx.recv())
                .await
                .is_err()
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), zks2_rx.recv())
                .await
                .is_err()
        );
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(100),
                unauthorized_rx.recv()
            )
            .await
            .is_err()
        );
    }

    #[test_log::test(tokio::test(flavor = "current_thread"))]
    async fn dispatch_verify_batch_returns_when_no_eligible_peers_exist() {
        let peer_sessions = Arc::new(RwLock::new(PeerSessionStore::default()));
        let connection_registry: ConnectionRegistry = Arc::new(RwLock::new(HashMap::new()));

        tokio::time::timeout(
            std::time::Duration::from_millis(250),
            dispatch_verify_batch(&peer_sessions, &connection_registry, verify_request()),
        )
        .await
        .expect("dispatch should return immediately when there are no eligible peers");
    }
}
