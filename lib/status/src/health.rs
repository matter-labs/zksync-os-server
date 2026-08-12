use crate::AppState;
use axum::Json;
use axum::http::StatusCode;
use serde::Serialize;

#[derive(Serialize)]
pub struct HealthResponse {
    healthy: bool,
}

pub(crate) async fn health(
    _state: axum::extract::State<AppState>,
) -> (StatusCode, Json<HealthResponse>) {
    (StatusCode::OK, Json(HealthResponse { healthy: true }))
}

pub(crate) async fn ready(state: axum::extract::State<AppState>) -> StatusCode {
    if state.ready.get().is_some() {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}
