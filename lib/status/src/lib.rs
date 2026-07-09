mod consensus;
mod health;
mod status;

use crate::health::health;
use crate::status::status;
use axum::extract::State;
use axum::http::StatusCode;
use axum::{Router, routing::get};
use reth_tasks::shutdown::GracefulShutdown;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;

pub use consensus::{
    ConsensusMetricsEncoder, ConsensusStatus, ConsensusStatusSource, FinalizedObservation,
    RegistryStatus,
};
pub use status::StatusResponse;

/// Serves the consensus runtime's own prometheus registry (engine, marshal, p2p). A
/// second scrape target beside the node's main metrics port; 404 on nodes without
/// consensus, 503 until the consensus runtime has come up.
async fn consensus_metrics(
    State(consensus): State<Arc<Option<ConsensusStatusSource>>>,
) -> Result<String, StatusCode> {
    let Some(source) = consensus.as_ref() else {
        return Err(StatusCode::NOT_FOUND);
    };
    let Some(encoder) = source.metrics_encoder.borrow().clone() else {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    };
    Ok(encoder())
}

// todo: handle graceful shutdown in a meaningful manner:
//       we should start a timer for RPC server's lifetime, report healthy=false and only shutdown
//       after timer is expired
pub async fn run_status_server(
    addr: SocketAddr,
    shutdown: GracefulShutdown,
    consensus: Option<ConsensusStatusSource>,
) -> anyhow::Result<()> {
    let app = Router::new()
        .route("/status/health", get(health))
        .route("/status", get(status))
        .route("/status/consensus-metrics", get(consensus_metrics))
        .with_state(Arc::new(consensus));

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
