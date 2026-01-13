use crate::config::NetworkConfig;
use crate::protocol::{ProtocolEvent, ProtocolState, ZksProtocolHandler};
use crate::version::ZksProtocolV1;
use reth_chainspec::ChainSpecProvider;
use reth_discv5::discv5;
use reth_eth_wire::HelloMessageWithProtocols;
use reth_net_nat::NatResolver;
use reth_network::error::NetworkError;
use reth_network::{NetworkConfig as RethNetworkConfig, NetworkManager};
use std::net::{SocketAddr, SocketAddrV4};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;
use zksync_os_metadata::NODE_CLIENT_VERSION;
use zksync_os_reth_compat::provider::ZkProviderFactory;
use zksync_os_storage_api::{ReadReplay, ReadRepository, ReadStateHistory, ReplayRecord};
use zksync_os_types::NodeRole;

/// Max number of active devp2p connections.
const MAX_ACTIVE_CONNECTIONS: usize = 10;

/// Manages the entire network state including all RLPx subprotocols and discv5 peer discovery.
///
/// This type is supposed to be consumed through [`NetworkService::run`] that registers it as an
/// endless task that consistently drives the state of the entire network forward.
#[derive(Debug)]
pub struct NetworkService {
    network_manager: NetworkManager,
    protocol_rx: mpsc::UnboundedReceiver<ProtocolEvent>,
}

impl NetworkService {
    pub async fn new(
        config: NetworkConfig,
        node_role: NodeRole,
        replay: impl ReadReplay + Clone,
        zk_provider_factory: ZkProviderFactory<
            impl ReadStateHistory + Clone,
            impl ReadRepository + Clone,
        >,
        replay_sender: mpsc::UnboundedSender<ReplayRecord>,
    ) -> Result<Self, NetworkError> {
        match NatResolver::Any.external_addr().await {
            None => {
                tracing::info!("could not resolve external IP (STUN)");
            }
            Some(ip) => {
                tracing::info!(%ip, "resolved external IP (STUN)");
            }
        };
        let rlpx_address = SocketAddr::V4(SocketAddrV4::new(config.address, config.port));
        let (protocol_tx, protocol_rx) = mpsc::unbounded_channel();
        let net_cfg = RethNetworkConfig::builder(config.secret_key)
            .boot_nodes(config.boot_nodes.clone())
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
                reth_discv5::Config::builder(rlpx_address).discv5_config(
                    discv5::ConfigBuilder::new(discv5::ListenConfig::from_ip(
                        rlpx_address.ip(),
                        config.port,
                    ))
                    .build(),
                ),
            )
            // Use the same port for RLPx (TCP) and for discv5 (UDP)
            .listener_addr(rlpx_address)
            .discovery_addr(rlpx_address)
            // Disable transaction gossip as it is unsupported by ZKsync OS
            .disable_tx_gossip(true)
            // Do not require any block hashes in `eth` RLPx protocol as it is unused
            .required_block_hashes(vec![])
            // Set network id to ZKsync OS chain's id, otherwise we might connect to unrelated peers
            .network_id(Some(zk_provider_factory.chain_spec().chain.id()))
            // Add latest version of `zks` subprotocol. In the future this can be extended so that
            // several versions are registered here.
            .add_rlpx_sub_protocol(ZksProtocolHandler::<ZksProtocolV1, _> {
                replay,
                // we only want to request blocks if this is external node
                to_request_blocks: node_role.is_external(),
                state: ProtocolState::new(protocol_tx, MAX_ACTIVE_CONNECTIONS),
                replay_sender,
                _phantom: Default::default(),
            })
            .build(zk_provider_factory.clone());
        tracing::debug!(?net_cfg, "starting p2p network service");
        // Create network manager. We are not interested in `txpool` because transaction gossip is
        // disabled. `request_handler` is also unused as it is specific to `eth` protocol.
        let (network_manager, _txpool, _request_handler) =
            NetworkManager::builder(net_cfg).await?.split();

        Ok(Self {
            network_manager,
            protocol_rx,
        })
    }

    /// Consume the service by registering it as an endless task that drives the network state.
    pub fn run(mut self, tasks: &mut JoinSet<()>, stop_receiver: watch::Receiver<bool>) {
        tasks.spawn(self.network_manager);
        tasks.spawn(async move {
            while !*stop_receiver.borrow() {
                let Some(event) = self.protocol_rx.recv().await else {
                    break;
                };
                // For now events are only used for diagnostical reasons (new connection got
                // established or max connections reached). In the future we might have other events
                // that we would want to process here somehow.
                tracing::trace!(?event, "received zks protocol event");
            }
        });
    }
}
