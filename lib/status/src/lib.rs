mod health;

use crate::health::health;
use axum::{Router, routing::get};
use std::net::SocketAddr;
use tokio::{net::TcpListener, sync::watch};

#[derive(Clone)]
struct AppState {
    stop_receiver: watch::Receiver<bool>,
}

pub async fn prepare_status_server(
    bind_address: String,
    stop_receiver: watch::Receiver<bool>,
) -> anyhow::Result<(TcpListener, Router)> {
    let app = Router::new()
        .route("/status/health", get(health))
        .with_state(AppState { stop_receiver });

    let addr: SocketAddr = bind_address.parse()?;
    let listener = TcpListener::bind(addr).await?;

    Ok((listener, app))
}

pub async fn run_status_server(listener: TcpListener, app: Router) -> anyhow::Result<()> {
    let addr = listener.local_addr()?;
    tracing::info!("running a status server" = %addr);

    axum::serve(listener, app).await?;

    Ok(())
}
