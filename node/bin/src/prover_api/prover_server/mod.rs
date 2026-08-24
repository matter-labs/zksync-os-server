//! Prover server module for handling proof generation requests.
//!
//! This module provides an HTTP server that manages proof generation jobs
//! and proof storage.
mod v1;

use std::sync::Arc;

use crate::prover_api::{
    fri_job_manager::FriJobManager, proof_storage::ProofStorage, prover_server::v1::v1_routes,
    prover_server::v1::zisk_routes, snark_job_manager::SnarkJobManager,
};

use axum::{Router, extract::DefaultBodyLimit};
use reth_tasks::shutdown::GracefulShutdown;
use tokio::net::TcpListener;

/// Application state shared across all request handlers.
#[derive(Clone)]
pub(in crate::prover_api::prover_server) struct AppState {
    fri_job_manager: Arc<FriJobManager>,
    snark_job_manager: Arc<SnarkJobManager>,
    zisk_job_manager: Option<Arc<zisk_prover_lane::ZiskJobManager>>,
    zisk_aggregation_job_manager: Option<Arc<zisk_prover_lane::ZiskAggregationJobManager>>,
    proof_storage: ProofStorage,
}

/// Runs the prover API HTTP server on a pre-bound listener.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    fri_job_manager: Arc<FriJobManager>,
    snark_job_manager: Arc<SnarkJobManager>,
    zisk_job_manager: Option<Arc<zisk_prover_lane::ZiskJobManager>>,
    zisk_aggregation_job_manager: Option<Arc<zisk_prover_lane::ZiskAggregationJobManager>>,
    proof_storage: ProofStorage,
    listener: TcpListener,
    shutdown: GracefulShutdown,
) {
    let app_state = AppState {
        fri_job_manager,
        snark_job_manager,
        zisk_job_manager,
        zisk_aggregation_job_manager,
        proof_storage,
    };

    let app = build_router(app_state);

    let addr = listener
        .local_addr()
        .expect("failed to get prover server local addr");
    tracing::info!("prover API server listening on {addr}");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown.ignore_guard())
        .await
        .expect("never errors according to doc");
}

/// Assemble the prover API router. The second proof-system routes are mounted
/// only when the ZiSK job manager exists (feature enabled). When it is absent
/// the router is byte-identical to upstream and every ZiSK path returns 404.
fn build_router(app_state: AppState) -> Router {
    let v1 = if app_state.zisk_job_manager.is_some() {
        v1_routes().merge(zisk_routes())
    } else {
        v1_routes()
    };
    Router::new()
        .nest("/prover-jobs/v1", v1)
        .with_state(app_state)
        .layer(DefaultBodyLimit::max(10 * 1024 * 1024))
}
