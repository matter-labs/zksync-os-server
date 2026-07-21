use crate::config::NetworkConfig;
use crate::protocol::{
    ExternalNodeProtocolConfig, HandlerSharedState, MainNodeProtocolConfig, ProtocolEvent,
    ZksProtocolConfig, ZksProtocolHandler,
};
use crate::raft::protocol::RaftProtocolHandler;
use crate::session::PeerSessionStore;
use crate::twofa::wire::Zks2faMessage;
use crate::twofa::{
    ExternalNode2faConfig, MainNode2faConfig, Zks2faConnectionRegistry, Zks2faProtocolHandler,
};
use crate::version::ZksProtocolV5;
use crate::{VerifyBatch, VerifyBatchResult};
use alloy::eips::eip2124::Head;
use backon::{ConstantBuilder, Retryable};
use futures::future::join_all;
use reth_chainspec::{ChainSpecProvider, EthChainSpec, Hardforks};
use reth_discv5::discv5;
use reth_eth_wire::HelloMessageWithProtocols;
use reth_network::error::NetworkError;
use reth_network::types::peers::config::PeerBackoffDurations;
use reth_network::{
    NetworkConfig as RethNetworkConfig, NetworkConfigBuilder, NetworkManager, PeersConfig,
};
use reth_network_peers::PeerId;
use reth_network_peers::{NodeRecord, TrustedPeer};
use reth_provider::BlockNumReader;
use reth_tasks::Runtime;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::io;
use std::net::{
    IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener as StdTcpListener, UdpSocket,
};
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
const EPHEMERAL_NETWORK_PORT_RESERVATION_ATTEMPTS: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetworkPorts {
    /// TCP port used by RLPx.
    pub tcp: u16,
    /// UDP port used by discv5.
    pub udp: u16,
}

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

#[derive(Debug)]
struct ReservedTcpUdpPort {
    port: u16,
    _tcp_listener: StdTcpListener,
    _udp_socket: UdpSocket,
}

fn try_reserve_tcp_udp_port(address: Ipv4Addr, port: u16) -> io::Result<ReservedTcpUdpPort> {
    let tcp_listener = StdTcpListener::bind(SocketAddrV4::new(address, port))?;
    let port = tcp_listener.local_addr()?.port();
    let udp_socket = UdpSocket::bind(SocketAddrV4::new(address, port))?;
    Ok(ReservedTcpUdpPort {
        port,
        _tcp_listener: tcp_listener,
        _udp_socket: udp_socket,
    })
}

fn reserve_ephemeral_tcp_udp_port(address: Ipv4Addr) -> io::Result<ReservedTcpUdpPort> {
    let mut last_error = None;
    for _ in 0..EPHEMERAL_NETWORK_PORT_RESERVATION_ATTEMPTS {
        match try_reserve_tcp_udp_port(address, 0) {
            Ok(reservation) => return Ok(reservation),
            Err(err) => last_error = Some(err),
        }
    }

    Err(last_error.unwrap_or_else(|| {
        io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            "no ephemeral TCP+UDP port reservation was attempted",
        )
    }))
}

fn network_error_is_addr_in_use(error: &NetworkError) -> bool {
    match error {
        NetworkError::AddressAlreadyInUse { .. } => true,
        NetworkError::Io(err) | NetworkError::Discovery(_, err) => {
            err.kind() == io::ErrorKind::AddrInUse
        }
        NetworkError::Discv5Error(err) => {
            // discv5 does not expose AddrInUse as an io::ErrorKind, so match its display text.
            let err = err.to_string().to_ascii_lowercase();
            err.contains("address already in use")
                || err.contains("addrinuse")
                || err.contains("os error 48")
                || err.contains("os error 98")
        }
        NetworkError::DnsResolver(_) => false,
    }
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

    let resolve_once = || {
        let state = Arc::clone(&state);
        async move {
            resolve_boot_nodes_once(&state, &|boot_node: TrustedPeer| async move {
                boot_node.resolve().await
            })
            .await
        }
    };
    resolve_once
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
    /// Registry of live `zks_2fa` connections used to dispatch `VerifyBatch` to verifier peers.
    zks_2fa_registry: Zks2faConnectionRegistry,
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
    /// Builds the network service and returns the TCP/UDP ports it bound.
    ///
    /// When `config.port` is 0, startup first finds a port available for both TCP and UDP, then
    /// retries if another process claims it before reth binds its sockets.
    pub async fn new<Replay, Client>(
        mut config: NetworkConfig,
        runtime: Runtime,
        protocol_config: ZksProtocolConfig,
        replay: Replay,
        client: Client,
        raft_handler: Option<RaftProtocolHandler>,
    ) -> Result<(Self, NetworkPorts), NetworkError>
    where
        Replay: ReadReplay + Clone,
        Client: ChainSpecProvider<ChainSpec: Hardforks> + BlockNumReader + Clone + 'static,
    {
        if config.port != 0 {
            return Self::build(
                config,
                runtime,
                protocol_config,
                replay,
                client,
                raft_handler,
            )
            .await;
        }

        // Retries the reserve -> drop -> reth-bind sequence, unlike the inner reservation retry.
        let mut last_error = None;
        for attempt in 1..=EPHEMERAL_NETWORK_PORT_RESERVATION_ATTEMPTS {
            let reservation =
                reserve_ephemeral_tcp_udp_port(config.address).map_err(NetworkError::from)?;
            config.port = reservation.port;
            // reth binds its own sockets, so release the temporary reservation before building.
            drop(reservation);

            match Self::build(
                config.clone(),
                runtime.clone(),
                protocol_config.clone(),
                replay.clone(),
                client.clone(),
                raft_handler.clone(),
            )
            .await
            {
                Ok(service) => return Ok(service),
                Err(err)
                    if network_error_is_addr_in_use(&err)
                        && attempt < EPHEMERAL_NETWORK_PORT_RESERVATION_ATTEMPTS =>
                {
                    tracing::info!(
                        port = config.port,
                        attempt,
                        %err,
                        "ephemeral p2p port was claimed before reth could bind; retrying"
                    );
                    config.port = 0;
                    last_error = Some(err);
                    tokio::task::yield_now().await;
                }
                Err(err) => return Err(err),
            }
        }

        Err(last_error.unwrap_or_else(|| {
            io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                "failed to bind an ephemeral TCP+UDP p2p port",
            )
            .into()
        }))
    }

    async fn build(
        config: NetworkConfig,
        runtime: Runtime,
        protocol_config: ZksProtocolConfig,
        replay: impl ReadReplay + Clone,
        client: impl ChainSpecProvider<ChainSpec: Hardforks> + BlockNumReader + 'static,
        raft_handler: Option<RaftProtocolHandler>,
    ) -> Result<(Self, NetworkPorts), NetworkError> {
        // Install ViseRecorder before creating the NetworkManager so that reth-network metrics
        // are captured. This must happen before `NetworkManager::builder()` because that is where
        // reth initializes its metric handles (via `Default::default()` on each metrics struct).
        crate::metrics::install_recorder();
        let rlpx_address = SocketAddr::new(IpAddr::V4(config.address), config.port);
        let configured_port = config.port;
        let discv5_listen_config = discv5::ListenConfig::Ipv4 {
            ip: config.address,
            port: config.port,
        };
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
        // ENs only sync/verify against trusted peers; a main node must still accept untrusted ENs.
        let trusted_nodes_only = matches!(protocol_config, ZksProtocolConfig::ExternalNode(_));
        let cfg_builder = RethNetworkConfig::builder(config.secret_key, runtime)
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
                        discv5::ConfigBuilder::new(discv5_listen_config)
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
            .listener_addr(rlpx_address)
            .peer_config(
                PeersConfig::default()
                    // Sets peer ban duration to 1 second, effectively disabling it
                    .with_ban_duration(Duration::from_secs(1))
                    // Keep backoff durations short so that consensus nodes reconnect quickly
                    // after a peer restart or a transient network glitch. Long backoffs would
                    // stall raft leader election and block transaction processing.
                    // (low = transient failure, medium = persistent failure, high = bad peer,
                    // max = cumulative cap)
                    .with_backoff_durations(PeerBackoffDurations {
                        low: Duration::from_secs(1),
                        medium: Duration::from_secs(2),
                        high: Duration::from_secs(5),
                        max: Duration::from_secs(10),
                    })
                    // Peers' fork id must match, otherwise we could discover peers from other
                    // chains.
                    .with_enforce_enr_fork_id(true)
                    // Treat boot nodes as trusted peers: always keep and redial them (e.g. an EN
                    // pinning the main node) so replay sync never gets stranded on non-serving peers.
                    .with_trusted_nodes(config.boot_nodes.clone())
                    .with_trusted_nodes_only(trusted_nodes_only),
            )
            .discovery_addr(rlpx_address)
            // Disable transaction gossip as it is unsupported by ZKsync OS
            .disable_tx_gossip(true)
            // Do not require any block hashes in `eth` RLPx protocol as it is unused
            .required_block_hashes(vec![])
            // Set network id to ZKsync OS chain's id, otherwise we might connect to unrelated peers
            .network_id(Some(chain_spec.chain_id()))
            // Use genesis as chain head
            .set_head(genesis);
        // Boot nodes double as trusted peers, exempt from the connection cap on outgoing dials.
        let trusted_peer_ids: HashSet<PeerId> =
            config.boot_nodes.iter().map(|peer| peer.id).collect();
        let zks_2fa_registry: Zks2faConnectionRegistry = Arc::new(RwLock::new(HashMap::new()));
        let mut cfg_builder = match protocol_config {
            ZksProtocolConfig::MainNode(protocol) => Self::register_main_node_rlpx_sub_protocols(
                cfg_builder,
                protocol,
                replay,
                protocol_tx,
                zks_2fa_registry.clone(),
                trusted_peer_ids,
            ),
            ZksProtocolConfig::ExternalNode(protocol) => {
                Self::register_external_node_rlpx_sub_protocols(
                    cfg_builder,
                    protocol,
                    replay,
                    protocol_tx,
                    zks_2fa_registry.clone(),
                    trusted_peer_ids,
                )
            }
        };
        if let Some(raft_handler) = raft_handler {
            cfg_builder = cfg_builder.add_rlpx_sub_protocol(raft_handler);
        }
        let net_cfg = cfg_builder.build(client);
        tracing::debug!(?net_cfg, "starting p2p network service");
        // Create network manager. We are not interested in `txpool` because transaction gossip is
        // disabled. `request_handler` is also unused as it is specific to `eth` protocol.
        let (network_manager, _txpool, _request_handler) =
            NetworkManager::builder(net_cfg).await?.split();

        let bound_tcp_port = network_manager.local_addr().port();
        let bound_udp_port = network_manager
            .handle()
            .discv5()
            .map(|discv5| discv5.local_port())
            .expect("discv5 must be configured for zksync-os networking");
        debug_assert_eq!(bound_tcp_port, configured_port);
        debug_assert_eq!(bound_udp_port, configured_port);
        Ok((
            Self {
                network_manager,
                protocol_rx,
                peer_sessions: Arc::new(RwLock::new(PeerSessionStore::default())),
                zks_2fa_registry,
            },
            NetworkPorts {
                tcp: bound_tcp_port,
                udp: bound_udp_port,
            },
        ))
    }

    fn register_main_node_rlpx_sub_protocols(
        builder: NetworkConfigBuilder,
        protocol: MainNodeProtocolConfig,
        replay: impl ReadReplay + Clone,
        protocol_tx: mpsc::UnboundedSender<ProtocolEvent>,
        zks_2fa_registry: Zks2faConnectionRegistry,
        trusted_peers: HashSet<PeerId>,
    ) -> NetworkConfigBuilder {
        let state = HandlerSharedState::new(
            protocol_tx.clone(),
            MAX_ACTIVE_CONNECTIONS,
            trusted_peers.clone(),
        );
        let twofa_config = MainNode2faConfig {
            accepted_verifier_signers: protocol.accepted_verifier_signers,
            verify_result_tx: protocol.verify_result_tx,
        };
        let twofa_state =
            HandlerSharedState::new(protocol_tx, MAX_ACTIVE_CONNECTIONS, trusted_peers);
        builder
            .add_rlpx_sub_protocol(ZksProtocolHandler::<ZksProtocolV5, _>::for_main_node(
                replay, state,
            ))
            .add_rlpx_sub_protocol(Zks2faProtocolHandler::for_main_node(
                twofa_config,
                twofa_state,
                zks_2fa_registry,
            ))
    }

    fn register_external_node_rlpx_sub_protocols(
        builder: NetworkConfigBuilder,
        protocol: ExternalNodeProtocolConfig,
        replay: impl ReadReplay + Clone,
        protocol_tx: mpsc::UnboundedSender<ProtocolEvent>,
        zks_2fa_registry: Zks2faConnectionRegistry,
        trusted_peers: HashSet<PeerId>,
    ) -> NetworkConfigBuilder {
        let state = HandlerSharedState::new(
            protocol_tx.clone(),
            MAX_ACTIVE_CONNECTIONS,
            trusted_peers.clone(),
        );
        // Only verifier ENs advertise `zks_2fa`; replay-only ENs leave `verification` unset.
        let twofa_config = protocol
            .verification
            .clone()
            .map(|verifier| ExternalNode2faConfig {
                signing_key: verifier.signing_key,
                verify_batch_tx: verifier.verify_batch_tx,
                outgoing_verify_results: verifier.outgoing_verify_results,
            });
        let builder = builder.add_rlpx_sub_protocol(
            ZksProtocolHandler::<ZksProtocolV5, _>::for_external_node(replay, protocol, state),
        );
        match twofa_config {
            Some(twofa_config) => {
                let twofa_state =
                    HandlerSharedState::new(protocol_tx, MAX_ACTIVE_CONNECTIONS, trusted_peers);
                builder.add_rlpx_sub_protocol(Zks2faProtocolHandler::for_external_node(
                    twofa_config,
                    twofa_state,
                    zks_2fa_registry,
                ))
            }
            None => builder,
        }
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
        let zks_2fa_registry = Arc::clone(&self.zks_2fa_registry);
        if let Some(mut verify_request_rx) = verify_request_rx {
            runtime.spawn_critical_task("p2p verify dispatcher", async move {
                while let Some(request) = verify_request_rx.recv().await {
                    dispatch_verify_batch(&peer_sessions, &zks_2fa_registry, request).await;
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
/// [`PeerSessionStore`] selects peers that are authenticated and replayed through the requested
/// batch. The live [`Zks2faConnectionRegistry`] then supplies a send handle; an otherwise eligible
/// peer is skipped if its `zks_2fa` lane has already closed.
async fn dispatch_verify_batch(
    peer_sessions: &Arc<RwLock<PeerSessionStore>>,
    zks_2fa_registry: &Zks2faConnectionRegistry,
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
        let zks_2fa_registry = zks_2fa_registry.read().unwrap();
        eligible_peers
            .into_iter()
            .map(|peer_id| {
                let outbound_tx = zks_2fa_registry
                    .get(&peer_id)
                    .map(|handle| handle.outbound_tx.clone());
                (peer_id, outbound_tx)
            })
            .collect()
    };
    let mut sent = 0usize;
    for (peer_id, outbound_tx) in dispatch_targets {
        let Some(outbound_tx) = outbound_tx else {
            tracing::warn!(
                peer_id = %peer_id,
                request_id = request.request_id,
                batch_number = request.batch_number,
                "skipping verify request: no eligible active connection"
            );
            continue;
        };
        let encoded = Zks2faMessage::VerifyBatch(request.clone()).encoded();
        if outbound_tx.send(encoded).await.is_err() {
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
    use super::dispatch_verify_batch;
    use super::resolve_boot_nodes_once;
    use crate::VerifyBatch;
    use crate::session::PeerSessionStore;
    use crate::twofa::wire::Zks2faMessage;
    use crate::twofa::{Zks2faConnectionRegistry, Zks2faPeerHandle};
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
    async fn dispatch_verify_batch_sends_only_to_authorized_caught_up_peers() {
        let eligible_peer = peer_id(0x11);
        let lagging_peer = peer_id(0x22);
        let unauthorized_peer = peer_id(0x44);
        let signer = Address::repeat_byte(0xAA);

        let mut store = PeerSessionStore::default();
        add_authorized_peer(&mut store, eligible_peer, 120, signer);
        add_authorized_peer(&mut store, lagging_peer, 119, signer);
        let now = Instant::now();
        store.insert(now, unauthorized_peer, socket_addr(30_368));
        store.replay_requested(unauthorized_peer, 1);
        store.replay_block_sent(now, unauthorized_peer, 120);
        store.verifier_unauthorized(unauthorized_peer, Some(signer));

        let peer_sessions = Arc::new(RwLock::new(store));
        let zks_2fa_registry: Zks2faConnectionRegistry = Arc::new(RwLock::new(HashMap::new()));

        let (eligible_tx, mut eligible_rx) = mpsc::channel(1);
        let (lagging_tx, mut lagging_rx) = mpsc::channel(1);
        let (unauthorized_tx, mut unauthorized_rx) = mpsc::channel(1);

        {
            let mut registry = zks_2fa_registry.write().unwrap();
            registry.insert(
                eligible_peer,
                Zks2faPeerHandle {
                    outbound_tx: eligible_tx,
                },
            );
            registry.insert(
                lagging_peer,
                Zks2faPeerHandle {
                    outbound_tx: lagging_tx,
                },
            );
            registry.insert(
                unauthorized_peer,
                Zks2faPeerHandle {
                    outbound_tx: unauthorized_tx,
                },
            );
        }

        let request = verify_request();
        dispatch_verify_batch(&peer_sessions, &zks_2fa_registry, request.clone()).await;

        let encoded =
            tokio::time::timeout(std::time::Duration::from_millis(250), eligible_rx.recv())
                .await
                .expect("eligible peer should receive verify request")
                .expect("eligible peer channel closed");
        let mut slice = encoded.as_ref();
        match Zks2faMessage::decode_message(&mut slice).unwrap() {
            Zks2faMessage::VerifyBatch(actual) => assert_eq!(actual, request),
            other => panic!("unexpected zks_2fa message dispatched: {other:?}"),
        }

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), lagging_rx.recv())
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
        let zks_2fa_registry: Zks2faConnectionRegistry = Arc::new(RwLock::new(HashMap::new()));

        tokio::time::timeout(
            std::time::Duration::from_millis(250),
            dispatch_verify_batch(&peer_sessions, &zks_2fa_registry, verify_request()),
        )
        .await
        .expect("dispatch should return immediately when there are no eligible peers");
    }

    #[test_log::test(tokio::test(flavor = "current_thread"))]
    async fn dispatch_verify_batch_skips_peers_without_zks_2fa_connection() {
        // An authorized, caught-up peer with no live `zks_2fa` connection is skipped; other
        // eligible peers still receive the request.
        let connected_peer = peer_id(0x11);
        let disconnected_peer = peer_id(0x22);
        let signer = Address::repeat_byte(0xAA);

        let mut store = PeerSessionStore::default();
        add_authorized_peer(&mut store, connected_peer, 120, signer);
        add_authorized_peer(&mut store, disconnected_peer, 120, signer);

        let peer_sessions = Arc::new(RwLock::new(store));
        let zks_2fa_registry: Zks2faConnectionRegistry = Arc::new(RwLock::new(HashMap::new()));

        let (twofa_tx, mut twofa_rx) = mpsc::channel(1);
        zks_2fa_registry.write().unwrap().insert(
            connected_peer,
            Zks2faPeerHandle {
                outbound_tx: twofa_tx,
            },
        );

        let request = verify_request();
        tokio::time::timeout(
            std::time::Duration::from_millis(250),
            dispatch_verify_batch(&peer_sessions, &zks_2fa_registry, request.clone()),
        )
        .await
        .expect("dispatch should not block on the disconnected peer");

        let encoded = tokio::time::timeout(std::time::Duration::from_millis(250), twofa_rx.recv())
            .await
            .expect("connected zks_2fa peer should receive verify request")
            .expect("connected zks_2fa peer channel closed");
        let mut slice = encoded.as_ref();
        match Zks2faMessage::decode_message(&mut slice).unwrap() {
            Zks2faMessage::VerifyBatch(actual) => assert_eq!(actual, request),
            other => panic!("unexpected zks_2fa message dispatched: {other:?}"),
        }
    }
}
