use std::ops::RangeInclusive;
use zksync_os_contract_interface::l1_discovery::L1State;
use zksync_os_types::NodeRole;

#[allow(dead_code)] // some fields are only used for logging (`Debug`)
#[derive(Debug, Clone)]
pub struct NodeStateOnStartup {
    pub node_role: NodeRole,
    pub l1_state: L1State,
    pub state_block_range_available: RangeInclusive<u64>,
    pub block_replay_storage_last_block: u64,
    pub tree_last_block: u64,
    pub repositories_persisted_block: u64,
    pub last_l1_committed_block: u64,
    pub last_l1_proved_block: u64,
    pub last_l1_executed_block: u64,
}

impl NodeStateOnStartup {
    pub fn assert_consistency(&self) {
        assert!(
            self.last_l1_committed_block >= self.last_l1_proved_block,
            "Last committed block ({}) is less than last proved block ({})",
            self.last_l1_committed_block,
            self.last_l1_proved_block,
        );
        assert!(
            self.last_l1_proved_block >= self.last_l1_executed_block,
            "Last proved block ({}) is less than last executed block ({})",
            self.last_l1_proved_block,
            self.last_l1_executed_block,
        );
    }
}
