use crate::consensus::{ConsensusStatus, ConsensusStatusSource};
use axum::Json;
use axum::extract::State;
use serde::Serialize;
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct StatusResponse {
    pub healthy: bool,
    /// Present only on nodes running BFT consensus.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consensus: Option<ConsensusStatus>,
}

pub(crate) async fn status(
    State(consensus): State<Arc<Option<ConsensusStatusSource>>>,
) -> Json<StatusResponse> {
    Json(StatusResponse {
        healthy: true,
        consensus: consensus.as_ref().as_ref().map(|source| source.snapshot()),
    })
}
