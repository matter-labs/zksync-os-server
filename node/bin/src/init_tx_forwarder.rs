use crate::config::Config;
use alloy::providers::{Provider, ProviderBuilder};
use anyhow::Context;
use std::collections::HashMap;
use tokio::sync::watch;
use zksync_os_raft::RaftConsensusStatus;
use zksync_os_rpc::{TxForwardEndpoint, TxForwarder};

pub async fn build_static_tx_forwarder(url: &str) -> TxForwarder {
    let provider = ProviderBuilder::new()
        .connect(url)
        .await
        .expect("could not connect to main node RPC")
        .erased();
    TxForwarder::static_target(TxForwardEndpoint::new(url.to_owned(), provider))
}

pub async fn build_consensus_tx_forwarder(
    config: &Config,
    status_rx: watch::Receiver<Option<RaftConsensusStatus>>,
) -> TxForwarder {
    let node_id = config
        .network_config
        .derived_peer_id()
        .expect("failed to derive local consensus peer id")
        .to_string();
    let mut providers = HashMap::new();
    for endpoint in &config.consensus_config.tx_forwarding_rpc_urls {
        let (peer_id, rpc_url) = parse_consensus_rpc_forwarder(endpoint)
            .unwrap_or_else(|err| panic!("invalid consensus tx RPC forwarder: {err:#}"));
        let provider = ProviderBuilder::new()
            .connect(&rpc_url)
            .await
            .unwrap_or_else(|err| {
                panic!("could not connect to consensus RPC {rpc_url} for peer {peer_id}: {err}")
            })
            .erased();
        providers.insert(peer_id, TxForwardEndpoint::new(rpc_url, provider));
    }
    for peer_id in &config.consensus_config.peer_ids {
        if !providers.contains_key(&peer_id.to_string()) {
            panic!("missing consensus tx RPC forwarder for peer {peer_id}");
        }
    }

    TxForwarder::consensus_leader(node_id, status_rx, providers)
}

fn parse_consensus_rpc_forwarder(endpoint: &str) -> anyhow::Result<(String, String)> {
    let endpoint = endpoint.trim();
    let endpoint = if endpoint.contains("://") {
        endpoint.to_owned()
    } else {
        format!("enode://{endpoint}")
    };
    let peer: zksync_os_network::TrustedPeer = endpoint.parse().with_context(
        || "expected `consensus.tx_forwarding_rpc_urls` entry as `<peer_id>@<host>:<rpc_port>`",
    )?;

    Ok((
        peer.id.to_string(),
        format!("http://{}:{}", peer.host, peer.tcp_port),
    ))
}
