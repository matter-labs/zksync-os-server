use reth_network::config::SecretKey;
use reth_network_peers::TrustedPeer;
use std::net::Ipv4Addr;

#[derive(Debug)]
pub struct NetworkConfig {
    /// The node's secret key, from which the node's identity is derived. Used during initial RLPx
    /// handshake.
    pub secret_key: SecretKey,
    /// IPv4 address to use for Node Discovery Protocol v5 (discv5) and RLPx Transport Protocol (rlpx).
    pub address: Ipv4Addr,
    /// Port to use for Node Discovery Protocol v5 (discv5) and RLPx Transport Protocol (rlpx).
    pub port: u16,
    /// All boot nodes to start network discovery with. Expected format is
    /// `enode://<node ID>@<IP address-or-DNS host>:<port>`.
    pub boot_nodes: Vec<TrustedPeer>,
}
