use crate::AppState;
use axum::Json;
use axum::http::StatusCode;
use serde::Serialize;

#[derive(Serialize)]
pub struct HealthResponse {
    healthy: bool,
}

pub(crate) async fn health(
    state: axum::extract::State<AppState>,
) -> (StatusCode, Json<HealthResponse>) {
    let is_terminating = *state.stop_receiver.borrow();

    let status = if is_terminating {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };

    (
        status,
        Json(HealthResponse {
            healthy: !is_terminating,
        }),
    )
}
