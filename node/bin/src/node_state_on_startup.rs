use std::ops::RangeInclusive;
use zksync_os_l1_sender::l1_discovery::L1State;

#[allow(dead_code)] // some fields are only used for logging (`Debug`)
#[derive(Debug, Clone)]
pub struct NodeStateOnStartup {
    pub l1_state: L1State,
    pub state_block_range_available: RangeInclusive<u64>,
    pub block_replay_storage_last_block: u64,
    pub tree_last_block: u64,
    pub repositories_persisted_block: u64,
    pub last_committed_block: u64,
    pub last_proved_block: u64,
    pub last_executed_block: u64,
}
