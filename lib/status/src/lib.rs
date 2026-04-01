mod health;
mod pipeline;

use crate::health::health;
use crate::pipeline::pipeline;
use axum::{Router, routing::get};
use reth_tasks::shutdown::GracefulShutdown;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::{net::TcpListener, sync::watch};
use zksync_os_observability::ComponentHealth;
use zksync_os_pipeline_health::{ComponentId, PipelineHealthConfig};
use zksync_os_types::TransactionAcceptanceState;

/// Outputs of setting up the pipeline health monitoring system.
/// Returned by `run_main_node_pipeline` and `run_en_pipeline`, then
/// used to construct [`StatusServerState`] for the status server.
#[derive(Clone)]
pub struct PipelineHealth {
    pub acceptance_rx: watch::Receiver<TransactionAcceptanceState>,
    pub component_health: Arc<Vec<(ComponentId, watch::Receiver<ComponentHealth>)>>,
    pub adjacency: Arc<Vec<(ComponentId, ComponentId)>>,
}

#[derive(Clone)]
pub struct StatusServerState {
    pub stop_receiver: watch::Receiver<bool>,
    pub acceptance_state: watch::Receiver<TransactionAcceptanceState>,
    pub component_health: Arc<Vec<(ComponentId, watch::Receiver<ComponentHealth>)>>,
    pub pipeline_health_config: PipelineHealthConfig,
    pub adjacency: Arc<Vec<(ComponentId, ComponentId)>>,
}

pub(crate) type AppState = StatusServerState;

pub async fn run_status_server(
    addr: SocketAddr,
    shutdown: GracefulShutdown,
    state: StatusServerState,
) {
    let app = Router::new()
        .route("/status/health", get(health))
        .route("/status/pipeline", get(pipeline))
        .with_state(state);

    let listener = TcpListener::bind(addr)
        .await
        .expect("cannot listen on address");

    let addr = listener.local_addr().expect("cannot get local address");
    tracing::info!(%addr, "status server running");

    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let graceful_guard = shutdown.await;
            tracing::info!("status server graceful shutdown complete");
            drop(graceful_guard);
        })
        .await
        .expect("never errors according to doc");
}
