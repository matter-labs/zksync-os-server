use axum::Json;
use serde::Serialize;

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct StatusResponse {
    pub healthy: bool,
}

pub(crate) async fn status() -> Json<StatusResponse> {
    Json(StatusResponse { healthy: true })
}
