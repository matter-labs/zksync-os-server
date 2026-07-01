use crate::AppState;
use axum::Json;
use serde::Serialize;
use zksync_os_raft::RaftConsensusStatus;

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct StatusResponse {
    pub healthy: bool,
    pub consensus: ConsensusStatus,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct ConsensusStatus {
    pub raft: Option<RaftConsensusStatus>,
}

pub(crate) async fn status(state: axum::extract::State<AppState>) -> Json<StatusResponse> {
    let consensus = ConsensusStatus {
        raft: state
            .consensus_raft_status_rx
            .as_ref()
            .and_then(|rx| rx.borrow().clone()),
    };

    Json(StatusResponse {
        healthy: true,
        consensus,
    })
}
