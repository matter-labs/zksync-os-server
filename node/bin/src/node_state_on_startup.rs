use crate::config::Config;
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

    /// Main-node-only guard against starting from a stale replay WAL.
    ///
    /// If the local WAL head is behind the last L1-committed block, the WAL is missing records
    /// for blocks that are already committed on L1. Starting the sequencer in this state makes it
    /// produce fresh blocks at WAL head + 1, re-sequencing block numbers L1 already knows —
    /// guaranteed divergence, followed by batcher/commit-watcher crashes. The replay archive is
    /// guaranteed complete up to the last L1-committed block (the archive gate blocks L1 commits
    /// until all blocks of the batch are archived), so the correct fix is offline recovery.
    ///
    /// Panics with a pointer to the recovery docs unless `general.allow_wal_behind_l1` is set.
    pub fn check_wal_not_behind_l1(&self, config: &Config) {
        if !self.node_role.is_main() {
            return;
        }
        if self.block_replay_storage_last_block >= self.last_l1_committed_block {
            return;
        }

        let msg = "local replay WAL is behind the last L1-committed block. Restore the WAL from the replay archive first, see https://github.com/matter-labs/zksync-os-server/blob/main/lib/replay_archive/README.md";

        if config.general_config.allow_wal_behind_l1 {
            tracing::warn!(
                "`general.allow_wal_behind_l1` is set - starting anyway and DIVERGING from L1. {msg}"
            );
        } else {
            panic!(
                "{msg}. To bypass this check and start anyway (divergence!), set \
                 `general.allow_wal_behind_l1=true` (env: `ALLOW_WAL_BEHIND_L1=true`)."
            );
        }
    }
}
