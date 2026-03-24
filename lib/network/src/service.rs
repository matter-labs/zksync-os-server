use crate::config::NetworkConfig;
use crate::protocol::{ProtocolEvent, ProtocolState, ZksProtocolHandler};
use crate::version::{ZksProtocolV1, ZksProtocolV2};
use crate::wire::replays::RecordOverride;
use alloy::eips::eip2124::Head;
use alloy::primitives::BlockNumber;
use reth_chainspec::{ChainSpecProvider, EthChainSpec, Hardforks};
use reth_discv5::discv5;
use reth_eth_wire::HelloMessageWithProtocols;
use reth_net_nat::NatResolver;
use reth_network::error::NetworkError;
use reth_network::types::peers::config::PeerBackoffDurations;
use reth_network::{
    NetworkConfig as RethNetworkConfig, NetworkConfigBuilder, NetworkManager, PeersConfig,
};
use reth_provider::BlockNumReader;
use reth_tasks::Runtime;
use std::net::{SocketAddr, SocketAddrV4};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::mpsc;
use zksync_os_metadata::NODE_CLIENT_VERSION;
use zksync_os_storage_api::{ReadReplay, ReplayRecord};
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

pub struct ZksProtocolConfig {
    pub node_role: NodeRole,
    pub starting_block: BlockNumber,
    pub record_overrides: Vec<RecordOverride>,
    pub replay_sender: mpsc::Sender<ReplayRecord>,
}

impl NetworkService {
    pub async fn new(
        config: NetworkConfig,
        zks_config: ZksProtocolConfig,
        replay: impl ReadReplay + Clone,
        client: impl ChainSpecProvider<ChainSpec: Hardforks> + BlockNumReader + 'static,
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
        let chain_spec = client.chain_spec();
        let genesis = Head {
            hash: chain_spec.genesis_hash(),
            number: 0,
            timestamp: chain_spec.genesis().timestamp,
            difficulty: chain_spec.genesis().difficulty,
            total_difficulty: chain_spec.genesis().difficulty,
        };
        let fork_id = chain_spec.fork_id(&genesis);
        tracing::info!(?genesis, ?fork_id, "initializing p2p network service");
        let (protocol_tx, protocol_rx) = mpsc::unbounded_channel();
        let cfg_builder = RethNetworkConfig::builder(config.secret_key)
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
        let net_cfg =
            Self::register_rlpx_sub_protocols(cfg_builder, zks_config, replay, protocol_tx)
                .build(client);
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

    fn register_rlpx_sub_protocols(
        builder: NetworkConfigBuilder,
        config: ZksProtocolConfig,
        replay: impl ReadReplay + Clone,
        protocol_tx: mpsc::UnboundedSender<ProtocolEvent>,
    ) -> NetworkConfigBuilder {
        // Shared between all `zks` versions. For example, if we replay first 1000 blocks using v1
        // and then start replaying using v2, we should respect those 1000 replay records we have
        // already received using v1.
        let starting_block = Arc::new(RwLock::new(config.starting_block));
        let state = ProtocolState::new(protocol_tx, MAX_ACTIVE_CONNECTIONS);
        builder
            // Support for v1 must be dropped before upgrade to protocol version v31.0. Otherwise,
            // we might send invalid record to ENs that are still using v1 protocol (`starting_migration_number`
            // in those record is always 0).
            .add_rlpx_sub_protocol(ZksProtocolHandler::<ZksProtocolV1, _>::new(
                replay.clone(),
                config.node_role,
                starting_block.clone(),
                config.record_overrides.clone(),
                state.clone(),
                config.replay_sender.clone(),
            ))
            .add_rlpx_sub_protocol(ZksProtocolHandler::<ZksProtocolV2, _>::new(
                replay,
                config.node_role,
                starting_block,
                config.record_overrides,
                state,
                config.replay_sender,
            ))
    }

    /// Consume the service by registering it as an endless task that drives the network state.
    pub fn spawn(mut self, runtime: &Runtime) {
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
        runtime.spawn_critical_task("p2p protocol logger", async move {
            while let Some(event) = self.protocol_rx.recv().await {
                // For now events are only used for diagnostical reasons (new connection got
                // established or max connections reached). In the future we might have other events
                // that we would want to process here somehow.
                tracing::trace!(?event, "received zks protocol event");
            }
        });
    }
}
