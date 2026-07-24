//! Status HTTP endpoints.
//!
//! - `GET /status` — general node status, including consensus state on nodes
//!   running BFT consensus.
//! - `GET /status/health` — liveness endpoint. Always 200 while the process is up.
//! - `GET /status/ready` — readiness probe: 200 once the RPC server is serving,
//!   503 while starting up.
//! - `GET /status/pipeline` — per-component backpressure and lag snapshot for
//!   diagnostics and dashboards.
//! - `GET /status/consensus-metrics` — the consensus runtime's own prometheus
//!   registry; 404 on nodes without consensus.

mod consensus;
mod health;
mod pipeline;
mod status;

use crate::health::{health, ready};
use crate::pipeline::pipeline;
use crate::status::status;
use axum::extract::State;
use axum::http::StatusCode;
use axum::{Router, routing::get};
use reth_tasks::shutdown::GracefulShutdown;
use std::sync::{Arc, OnceLock};
use tokio::{net::TcpListener, sync::watch};
use zksync_os_backpressure::PipelineSnapshot;

pub use consensus::{
    ConsensusMetricsEncoder, ConsensusStatus, ConsensusStatusSource, FinalizedObservation,
    RegistryStatus, ScheduledCutoverStatus, ScheduledCutoverStatusSource,
};
pub use status::StatusResponse;

#[derive(Clone)]
pub struct StatusServerState {
    pub pipeline_snapshot: watch::Receiver<PipelineSnapshot>,
    /// Present only on nodes running BFT consensus (`Arc` because the source is
    /// not clonable while axum state must be).
    pub consensus: Arc<Option<ConsensusStatusSource>>,
    /// Present only while a consensus start is scheduled at a future height.
    pub scheduled_cutover: Option<ScheduledCutoverStatusSource>,
    pub ready: Arc<OnceLock<()>>,
}

pub(crate) type AppState = StatusServerState;

/// Serves the consensus runtime's own prometheus registry (engine, marshal, p2p). A
/// second scrape target beside the node's main metrics port; 404 on nodes without
/// consensus, 503 until the consensus runtime has come up.
async fn consensus_metrics(State(state): State<AppState>) -> Result<String, StatusCode> {
    let Some(source) = state.consensus.as_ref() else {
        return Err(StatusCode::NOT_FOUND);
    };
    let Some(encoder) = source.metrics_encoder.borrow().clone() else {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    };
    Ok(encoder())
}

/// Runs the status HTTP server on a pre-bound listener.
pub async fn run_status_server(
    listener: TcpListener,
    shutdown: GracefulShutdown,
    state: StatusServerState,
) -> anyhow::Result<()> {
    let app = Router::new()
        .route("/status", get(status))
        .route("/status/health", get(health))
        .route("/status/ready", get(ready))
        .route("/status/pipeline", get(pipeline))
        .route("/status/consensus-metrics", get(consensus_metrics))
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
