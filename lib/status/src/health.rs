use axum::Json;
use axum::http::StatusCode;
use serde::Serialize;

#[derive(Serialize)]
pub struct HealthResponse {
    healthy: bool,
}

pub(crate) async fn health() -> (StatusCode, Json<HealthResponse>) {
    (StatusCode::OK, Json(HealthResponse { healthy: true }))
}
