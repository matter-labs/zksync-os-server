use crate::config::Config;
use anyhow::Context;
use std::net::SocketAddr;
use tokio::net::TcpListener;
use zksync_os_network::NetworkPorts;

/// Actual ports bound by each service after `run()` starts.
/// Fields are `None` when the corresponding service is disabled in config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerPorts {
    pub rpc: u16,
    pub status: Option<u16>,
    pub prover_api: Option<u16>,
    pub network: Option<NetworkPorts>,
}

/// Sockets bound before node startup and then handed to their servers.
#[derive(Debug)]
pub(crate) struct BoundListeners {
    pub(crate) rpc: TcpListener,
    pub(crate) status: Option<TcpListener>,
    pub(crate) prover_api: Option<TcpListener>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Service {
    Rpc,
    Status,
    ProverApi,
}

impl std::fmt::Display for Service {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let label = match self {
            Service::Rpc => "RPC",
            Service::Status => "status",
            Service::ProverApi => "prover API",
        };
        write!(f, "{label}")
    }
}

impl BoundListeners {
    pub(crate) async fn bind_from_config(config: &Config) -> anyhow::Result<Self> {
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
        let rpc = bind_tcp_listener(rpc_address, Service::Rpc).await?;
        let status = match status_address {
            Some(address) => Some(bind_tcp_listener(address, Service::Status).await?),
            None => None,
        };
        let prover_api = match prover_api_address {
            Some(address) => Some(bind_tcp_listener(address, Service::ProverApi).await?),
            None => None,
        };

        Ok(Self {
            rpc,
            status,
            prover_api,
        })
    }
}

async fn bind_tcp_listener(address: &str, service: Service) -> anyhow::Result<TcpListener> {
    let addr: SocketAddr = address
        .parse()
        .with_context(|| format!("malformed {service} bind address {address:?}"))?;
    TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to prebind {service} listener at {address}"))
}
