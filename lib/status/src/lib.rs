//! Status HTTP endpoints.
//!
//! - `GET /status` — general node status, including consensus (Raft) state.
//! - `GET /status/health` — liveness endpoint. Always 200 while the process is up.
//! - `GET /status/pipeline` — per-component backpressure and lag snapshot for
//!   diagnostics and dashboards.

mod health;
mod pipeline;
mod status;

use crate::health::health;
use crate::pipeline::pipeline;
use crate::status::status;
use axum::{Router, routing::get};
use reth_tasks::shutdown::GracefulShutdown;
use tokio::{net::TcpListener, sync::watch};
use zksync_os_backpressure::PipelineSnapshot;
use zksync_os_raft::RaftConsensusStatus;

pub use status::{ConsensusStatus, StatusResponse};

#[derive(Clone)]
pub struct StatusServerState {
    pub pipeline_snapshot: watch::Receiver<PipelineSnapshot>,
    pub consensus_raft_status_rx: Option<watch::Receiver<Option<RaftConsensusStatus>>>,
}

pub(crate) type AppState = StatusServerState;

/// Runs the status HTTP server on a pre-bound listener.
pub async fn run_status_server(
    listener: TcpListener,
    shutdown: GracefulShutdown,
    state: StatusServerState,
) -> anyhow::Result<()> {
    let app = Router::new()
        .route("/status", get(status))
        .route("/status/health", get(health))
        .route("/status/pipeline", get(pipeline))
        .with_state(state);

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
