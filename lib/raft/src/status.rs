use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RaftConsensusStatus {
    pub node_id: String,
    pub state: String,
    pub is_leader: bool,
    pub current_leader: Option<String>,
    pub current_term: u64,
    pub last_applied_index: Option<u64>,
}
