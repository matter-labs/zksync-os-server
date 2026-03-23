mod health;
mod status;

use crate::health::health;
use crate::status::status;
use axum::{routing::get, Router};
use reth_tasks::shutdown::GracefulShutdown;
use std::net::SocketAddr;
use tokio::{net::TcpListener, sync::watch};
use zksync_os_raft::RaftConsensusStatus;

pub use status::{ConsensusStatus, StatusResponse};

#[derive(Clone)]
struct AppState {
    consensus_raft_status_rx: Option<watch::Receiver<RaftConsensusStatus>>,
}

// todo: handle graceful shutdown in a meaningful manner:
//       we should start a timer for RPC server's lifetime, report healthy=false and only shutdown
//       after timer is expired
pub async fn run_status_server(
    addr: SocketAddr,
    shutdown: GracefulShutdown,
    consensus_raft_status_rx: Option<watch::Receiver<RaftConsensusStatus>>,
) -> anyhow::Result<()> {
    let app = Router::new()
        .route("/status/health", get(health))
        .route("/status", get(status))
        .with_state(AppState {
            consensus_raft_status_rx,
        });

    let listener = TcpListener::bind(addr).await?;

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
