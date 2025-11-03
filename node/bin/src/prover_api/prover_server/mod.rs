//! Prover server module for handling proof generation requests.
//!
//! This module provides an HTTP server that manages proof generation jobs
//! and proof storage. It supports both legacy (to be deprecated end of Q4 2025)
//! and v1 (adds support for VKs and VK filtering) API routes for prover job management.
mod legacy;
mod v1;

use std::{net::SocketAddr, sync::Arc};

use crate::prover_api::{
    fri_job_manager::FriJobManager,
    proof_storage::ProofStorage,
    prover_server::{legacy::legacy_routes, v1::v1_routes},
    snark_job_manager::SnarkJobManager,
};

use axum::{Router, extract::DefaultBodyLimit};
use tokio::net::TcpListener;

/// Application state shared across all request handlers.
#[derive(Clone)]
pub(in crate::prover_api::prover_server) struct AppState {
    fri_job_manager: Arc<FriJobManager>,
    snark_job_manager: Arc<SnarkJobManager>,
    proof_storage: ProofStorage,
}

/// Entry point for prover API server.
/// Starts an HTTP server listening on the specified bind address.
pub async fn run(
    fri_job_manager: Arc<FriJobManager>,
    snark_job_manager: Arc<SnarkJobManager>,
    proof_storage: ProofStorage,
    bind_address: String,
) -> anyhow::Result<()> {
    let app_state = AppState {
        fri_job_manager,
        snark_job_manager,
        proof_storage,
    };

    let app = Router::new()
        .nest("/prover-jobs", legacy_routes())
        .nest("/prover-jobs/v1", v1_routes())
        .with_state(app_state)
        // Set the request body limit to 10MiB
        .layer(DefaultBodyLimit::max(10 * 1024 * 1024));

    let bind_address: SocketAddr = bind_address.parse()?;
    tracing::info!("starting proof data server on {bind_address}");

    let listener = TcpListener::bind(bind_address).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
