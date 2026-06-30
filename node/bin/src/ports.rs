use crate::config::Config;
use anyhow::Context;
use std::net::SocketAddr;
use tokio::net::TcpListener;
use zksync_os_network::NetworkPorts;

/// Actual ports bound by each service after `run()` starts.
/// Fields are `None` when the corresponding service is disabled in config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoundPorts {
    pub rpc: u16,
    pub status: Option<u16>,
    pub prover_api: Option<u16>,
    pub network: Option<NetworkPorts>,
}

/// Sockets bound before node startup and then handed to their servers.
#[derive(Debug)]
pub struct PreboundPorts {
    pub(crate) rpc: TcpListener,
    pub(crate) status: Option<TcpListener>,
    pub(crate) prover_api: Option<TcpListener>,
}

impl PreboundPorts {
    pub async fn bind_from_config(config: &Config) -> anyhow::Result<Self> {
        let status_address = config
            .status_server_config
            .enabled
            .then_some(config.status_server_config.address.as_str());
        let prover_api_address = (config.general_config.node_role.is_main()
            && config.batcher_config.enabled
            && config.prover_api_config.enabled)
            .then_some(config.prover_api_config.address.as_str());

        Self::bind(
            &config.rpc_config.address,
            status_address,
            prover_api_address,
        )
        .await
    }

    async fn bind(
        rpc_address: &str,
        status_address: Option<&str>,
        prover_api_address: Option<&str>,
    ) -> anyhow::Result<Self> {
        let rpc = bind_tcp_listener(rpc_address, "RPC").await?;
        let status = match status_address {
            Some(address) => Some(bind_tcp_listener(address, "status").await?),
            None => None,
        };
        let prover_api = match prover_api_address {
            Some(address) => Some(bind_tcp_listener(address, "prover API").await?),
            None => None,
        };

        Ok(Self {
            rpc,
            status,
            prover_api,
        })
    }

    pub fn bound_ports(&self) -> BoundPorts {
        BoundPorts {
            rpc: self.rpc.local_addr().expect("rpc server local_addr").port(),
            status: self.status.as_ref().map(|listener| {
                listener
                    .local_addr()
                    .expect("status server local_addr")
                    .port()
            }),
            prover_api: self.prover_api.as_ref().map(|listener| {
                listener
                    .local_addr()
                    .expect("prover API server local_addr")
                    .port()
            }),
            network: None,
        }
    }
}

async fn bind_tcp_listener(address: &str, service_name: &str) -> anyhow::Result<TcpListener> {
    let addr: SocketAddr = address
        .parse()
        .with_context(|| format!("malformed {service_name} bind address {address:?}"))?;
    TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to prebind {service_name} listener at {address}"))
}
