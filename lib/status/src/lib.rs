mod health;
mod status;

use crate::health::health;
use crate::status::status;
use axum::{Router, routing::get};
use reth_tasks::shutdown::GracefulShutdown;
use std::net::SocketAddr;
use tokio::net::TcpListener;

pub use status::StatusResponse;

// todo: handle graceful shutdown in a meaningful manner:
//       we should start a timer for RPC server's lifetime, report healthy=false and only shutdown
//       after timer is expired
pub async fn run_status_server(addr: SocketAddr, shutdown: GracefulShutdown) -> anyhow::Result<()> {
    let app = Router::new()
        .route("/status/health", get(health))
        .route("/status", get(status));

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
