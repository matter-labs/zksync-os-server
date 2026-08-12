//! Prover server module for handling proof generation requests.
//!
//! This module provides an HTTP server that manages proof generation jobs
//! and proof storage.
mod v1;

use std::sync::Arc;

use crate::prover_api::{
    fri_job_manager::FriJobManager, proof_storage::ProofStorage, prover_server::v1::v1_routes,
    snark_job_manager::SnarkJobManager,
};

use axum::{Router, extract::DefaultBodyLimit};
use reth_tasks::shutdown::GracefulShutdown;
use tokio::net::TcpListener;

/// Application state shared across all request handlers.
#[derive(Clone)]
pub(in crate::prover_api::prover_server) struct AppState {
    fri_job_manager: Arc<FriJobManager>,
    snark_job_manager: Arc<SnarkJobManager>,
    proof_storage: ProofStorage,
}

/// Runs the prover API HTTP server on a pre-bound listener.
pub async fn run(
    fri_job_manager: Arc<FriJobManager>,
    snark_job_manager: Arc<SnarkJobManager>,
    proof_storage: ProofStorage,
    listener: TcpListener,
    shutdown: GracefulShutdown,
) {
    let app_state = AppState {
        fri_job_manager,
        snark_job_manager,
        proof_storage,
    };

    let app = Router::new()
        .nest("/prover-jobs/v1", v1_routes())
        .with_state(app_state)
        .layer(DefaultBodyLimit::max(10 * 1024 * 1024));

    let addr = listener
        .local_addr()
        .expect("failed to get prover server local addr");
    tracing::info!("prover API server listening on {addr}");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown.ignore_guard())
        .await
        .expect("never errors according to doc");
}
