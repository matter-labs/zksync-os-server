use crate::AppState;
use crate::consensus::{ConsensusStatus, ScheduledCutoverStatus};
use axum::Json;
use axum::extract::State;
use serde::Serialize;

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct StatusResponse {
    pub healthy: bool,
    /// Present only on nodes running BFT consensus.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consensus: Option<ConsensusStatus>,
    /// Present only while a consensus start is scheduled at a future height.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheduled_cutover: Option<ScheduledCutoverStatus>,
}

pub(crate) async fn status(State(state): State<AppState>) -> Json<StatusResponse> {
    Json(StatusResponse {
        healthy: true,
        consensus: state
            .consensus
            .as_ref()
            .as_ref()
            .map(|source| source.snapshot()),
        scheduled_cutover: state
            .scheduled_cutover
            .as_ref()
            .map(|source| source.snapshot()),
    })
}
