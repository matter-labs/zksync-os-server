use std::ops::RangeInclusive;
use zksync_os_contract_interface::l1_discovery::L1State;

#[allow(dead_code)] // some fields are only used for logging (`Debug`)
#[derive(Debug, Clone)]
pub struct NodeStateOnStartup {
    pub is_main_node: bool,
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
        if self.is_main_node {
            assert!(
                self.block_replay_storage_last_block >= self.last_l1_committed_block,
                "Not all committed blocks are present in the block replay storage (last committed: {}, last in storage: {})",
                self.last_l1_committed_block,
                self.block_replay_storage_last_block,
            );
        }
    }
}
