use crate::config::NetworkConfig;
use crate::protocol::{ProtocolEvent, ProtocolState, ZksProtocolHandler};
use crate::version::{ZksProtocolV1, ZksProtocolV2};
use crate::wire::replays::RecordOverride;
use alloy::eips::eip2124::Head;
use alloy::primitives::BlockNumber;
use futures::FutureExt;
use futures::future::{BoxFuture, join_all};
use reth_chainspec::{ChainSpecProvider, EthChainSpec, Hardforks};
use reth_discv5::discv5;
use reth_eth_wire::HelloMessageWithProtocols;
use reth_net_nat::NatResolver;
use reth_network::error::NetworkError;
use reth_network::types::peers::config::PeerBackoffDurations;
use reth_network::{
    NetworkConfig as RethNetworkConfig, NetworkConfigBuilder, NetworkManager, PeersConfig,
};
use reth_network_peers::{NodeRecord, TrustedPeer};
use reth_provider::BlockNumReader;
use reth_tasks::Runtime;
use std::io;
use std::net::{SocketAddr, SocketAddrV4};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::mpsc;
use zksync_os_metadata::NODE_CLIENT_VERSION;
use zksync_os_storage_api::{ReadReplay, ReplayRecord};
use zksync_os_types::NodeRole;

/// Max number of active devp2p connections.
const MAX_ACTIVE_CONNECTIONS: usize = 10;
/// Retry DNS resolution aggressively during startup so discv5 bootstrap has usable boot nodes.
const INITIAL_BOOT_NODE_RESOLUTION_BACKOFF: Duration = Duration::from_secs(1);
/// Keep the retry interval bounded to avoid unbounded startup delays between attempts.
const MAX_BOOT_NODE_RESOLUTION_BACKOFF: Duration = Duration::from_secs(30);

async fn resolve_boot_nodes_with_backoff(
    boot_nodes: Vec<TrustedPeer>,
) -> Result<Vec<TrustedPeer>, NetworkError> {
    resolve_boot_nodes_with_backoff_using(
        boot_nodes,
        |boot_node| {
            async move {
                tokio::task::spawn_blocking(move || boot_node.resolve_blocking())
                    .await
                    .unwrap_or_else(|error| {
                        Err(io::Error::other(format!(
                            "boot node DNS resolution task failed: {error}"
                        )))
                    })
            }
            .boxed()
        },
        |duration| async move { tokio::time::sleep(duration).await }.boxed(),
    )
    .await
}

async fn resolve_boot_nodes_with_backoff_using<Resolve, Sleep>(
    boot_nodes: Vec<TrustedPeer>,
    resolve: Resolve,
    sleep: Sleep,
) -> Result<Vec<TrustedPeer>, NetworkError>
where
    Resolve: Fn(TrustedPeer) -> BoxFuture<'static, io::Result<NodeRecord>>,
    Sleep: Fn(Duration) -> BoxFuture<'static, ()>,
{
    if boot_nodes.is_empty() {
        return Ok(vec![]);
    }

    let mut unresolved_boot_nodes = boot_nodes;
    let mut resolved_boot_nodes = Vec::with_capacity(unresolved_boot_nodes.len());
    let mut backoff = INITIAL_BOOT_NODE_RESOLUTION_BACKOFF;

    loop {
        let resolution_results = join_all(unresolved_boot_nodes.into_iter().map(|boot_node| {
            let resolution = resolve(boot_node.clone());
            async move { (boot_node, resolution.await) }
        }))
        .await;

        unresolved_boot_nodes = Vec::new();
        for (boot_node, resolution) in resolution_results {
            match resolution {
                Ok(record) => {
                    tracing::info!(boot_node = %boot_node, resolved = ?record, "resolved boot node");
                    resolved_boot_nodes.push(record.into());
                }
                Err(err) => {
                    tracing::warn!(boot_node = %boot_node, %err, "failed to resolve boot node");
                    unresolved_boot_nodes.push(boot_node);
                }
            }
        }

        if unresolved_boot_nodes.is_empty() {
            return Ok(resolved_boot_nodes);
        }

        if !resolved_boot_nodes.is_empty() {
            tracing::warn!(
                resolved_boot_nodes = resolved_boot_nodes.len(),
                unresolved_boot_nodes = unresolved_boot_nodes.len(),
                "starting p2p network with partially resolved boot nodes"
            );
            return Ok(resolved_boot_nodes);
        }

        tracing::info!(
            retry_in = ?backoff,
            unresolved_boot_nodes = unresolved_boot_nodes.len(),
            "retrying boot node resolution before starting p2p network"
        );
        sleep(backoff).await;
        backoff = backoff
            .saturating_mul(2)
            .min(MAX_BOOT_NODE_RESOLUTION_BACKOFF);
    }
}

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
        let boot_nodes = resolve_boot_nodes_with_backoff(config.boot_nodes.clone()).await?;
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

#[cfg(test)]
mod tests {
    use super::resolve_boot_nodes_with_backoff_using;
    use futures::FutureExt;
    use reth_network_peers::{NodeRecord, TrustedPeer};
    use std::collections::{HashMap, VecDeque};
    use std::io;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

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

        let resolved = resolve_boot_nodes_with_backoff_using(
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
                    .boxed()
                }
            },
            {
                let sleeps = Arc::clone(&sleeps);
                move |duration| {
                    let sleeps = Arc::clone(&sleeps);
                    async move {
                        sleeps.lock().unwrap().push(duration);
                    }
                    .boxed()
                }
            },
        )
        .await
        .unwrap();

        assert_eq!(resolved, vec![trusted_peer(NODE_A_IP)]);
        assert_eq!(&*sleeps.lock().unwrap(), &[Duration::from_secs(1)]);
    }

    #[test_log::test(tokio::test(flavor = "current_thread"))]
    async fn boot_node_resolution_caps_exponential_backoff() {
        let responses = Arc::new(Mutex::new(HashMap::from([(
            NODE_A.to_owned(),
            VecDeque::from([
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                Some(node_record(NODE_A_IP)),
            ]),
        )])));
        let sleeps = Arc::new(Mutex::new(Vec::new()));

        let resolved = resolve_boot_nodes_with_backoff_using(
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
                    .boxed()
                }
            },
            {
                let sleeps = Arc::clone(&sleeps);
                move |duration| {
                    let sleeps = Arc::clone(&sleeps);
                    async move {
                        sleeps.lock().unwrap().push(duration);
                    }
                    .boxed()
                }
            },
        )
        .await
        .unwrap();

        assert_eq!(resolved, vec![trusted_peer(NODE_A_IP)]);
        assert_eq!(
            &*sleeps.lock().unwrap(),
            &[
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(4),
                Duration::from_secs(8),
                Duration::from_secs(16),
                Duration::from_secs(30),
                Duration::from_secs(30),
            ],
        );
    }

    #[test_log::test(tokio::test(flavor = "current_thread"))]
    async fn boot_node_resolution_returns_immediately_when_all_nodes_resolve() {
        let sleeps = Arc::new(Mutex::new(Vec::new()));

        let resolved = resolve_boot_nodes_with_backoff_using(
            vec![trusted_peer(NODE_A), trusted_peer(NODE_B)],
            |boot_node| {
                let record = match boot_node.to_string().as_str() {
                    NODE_A => node_record(NODE_A_IP),
                    NODE_B => node_record(NODE_B_IP),
                    _ => panic!("unexpected boot node"),
                };
                async move { Ok(record) }.boxed()
            },
            {
                let sleeps = Arc::clone(&sleeps);
                move |duration| {
                    let sleeps = Arc::clone(&sleeps);
                    async move {
                        sleeps.lock().unwrap().push(duration);
                    }
                    .boxed()
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
}
