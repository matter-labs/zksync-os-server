use crate::AppState;
use crate::consensus::ConsensusStatus;
use axum::Json;
use axum::extract::State;
use serde::Serialize;

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct StatusResponse {
    pub healthy: bool,
    /// Present only on nodes running BFT consensus.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consensus: Option<ConsensusStatus>,
}

pub(crate) async fn status(State(state): State<AppState>) -> Json<StatusResponse> {
    Json(StatusResponse {
        healthy: true,
        consensus: state
            .consensus
            .as_ref()
            .as_ref()
            .map(|source| source.snapshot()),
    })
}
