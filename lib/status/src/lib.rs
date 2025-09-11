mod health;

use crate::health::health;
use axum::{Router, routing::get};
use tokio::{
    net::TcpListener,
    sync::{oneshot, watch},
};

#[derive(Clone)]
struct AppState {
    stop_receiver: watch::Receiver<bool>,
}

pub async fn run_status_server(
    port_sink: oneshot::Sender<u16>,
    stop_receiver: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let app = Router::new()
        .route("/status/health", get(health))
        .with_state(AppState { stop_receiver });

    let listener = TcpListener::bind(("0.0.0.0", 0)).await?;

    let addr = listener.local_addr()?;
    tracing::info!("running a status server" = %addr);
    port_sink.send(addr.port()).unwrap();

    axum::serve(listener, app).await?;

    Ok(())
}
