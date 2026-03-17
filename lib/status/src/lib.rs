mod health;

use crate::health::health;
use axum::{Router, routing::get};
use reth_tasks::shutdown::GracefulShutdown;
use std::net::SocketAddr;
use tokio::net::TcpListener;

#[derive(Clone)]
struct AppState {}

// todo: handle graceful shutdown in a meaningful manner:
//       we should start a timer for RPC server's lifetime, report healthy=false and only shutdown
//       after timer is expired
pub async fn run_status_server(addr: SocketAddr, shutdown: GracefulShutdown) {
    let app = Router::new()
        .route("/status/health", get(health))
        .with_state(AppState {});

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
