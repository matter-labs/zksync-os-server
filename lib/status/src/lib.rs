mod health;
mod status;

use crate::health::health;
use crate::status::status;
use axum::{Router, routing::get};
use reth_tasks::shutdown::GracefulShutdown;
use std::net::SocketAddr;
use tokio::{net::TcpListener, sync::watch};
use zksync_os_raft::RaftConsensusStatus;

pub use status::{ConsensusStatus, StatusResponse};

#[derive(Clone)]
struct AppState {
    consensus_raft_status_rx: Option<watch::Receiver<Option<RaftConsensusStatus>>>,
}

// todo: handle graceful shutdown in a meaningful manner:
//       we should start a timer for RPC server's lifetime, report healthy=false and only shutdown
//       after timer is expired
pub async fn run_status_server(
    addr: SocketAddr,
    shutdown: GracefulShutdown,
    consensus_raft_status_rx: Option<watch::Receiver<Option<RaftConsensusStatus>>>,
) -> anyhow::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    run_status_server_on_listener(listener, shutdown, consensus_raft_status_rx).await
}

pub async fn run_status_server_on_listener(
    listener: TcpListener,
    shutdown: GracefulShutdown,
    consensus_raft_status_rx: Option<watch::Receiver<Option<RaftConsensusStatus>>>,
) -> anyhow::Result<()> {
    let app = Router::new()
        .route("/status/health", get(health))
        .route("/status", get(status))
        .with_state(AppState {
            consensus_raft_status_rx,
        });

    let addr = listener.local_addr()?;
    tracing::info!(%addr, "status server running");

    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let graceful_guard = shutdown.await;
            tracing::info!("status server graceful shutdown complete");
            drop(graceful_guard);
        })
        .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use reth_tasks::{RuntimeBuilder, RuntimeConfig, TokioConfig};
    use std::net::{Ipv4Addr, SocketAddrV4};
    use tokio::net::TcpListener;
    use tokio::runtime::Handle;

    #[tokio::test]
    async fn status_server_on_listener_responds() {
        let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let runtime = RuntimeBuilder::new(
            RuntimeConfig::default().with_tokio(TokioConfig::existing_handle(Handle::current())),
        )
        .build()
        .unwrap();
        runtime.spawn_critical_with_graceful_shutdown_signal(
            "status server",
            |shutdown| async move {
                run_status_server_on_listener(listener, shutdown, None)
                    .await
                    .unwrap();
            },
        );
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let resp = loop {
            match reqwest::get(format!("http://localhost:{port}/status")).await {
                Ok(resp) => break resp,
                Err(err) if tokio::time::Instant::now() < deadline => {
                    tracing::debug!(%err, "status server not ready yet");
                    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                }
                Err(err) => panic!("status server did not become ready: {err}"),
            }
        };
        assert_eq!(resp.status(), 200);
    }
}
