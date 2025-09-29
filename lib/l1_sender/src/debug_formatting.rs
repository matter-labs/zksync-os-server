use crate::commitment::CommitBatchInfo;
use std::fmt;

impl fmt::Debug for CommitBatchInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CommitBatchInfo")
            .field("batch_number", &self.batch_number)
            .field("new_state_commitment", &self.new_state_commitment)
            .field("number_of_layer1_txs", &self.number_of_layer1_txs)
            .field("priority_operations_hash", &self.priority_operations_hash)
            .field(
                "dependency_roots_rolling_hash",
                &self.dependency_roots_rolling_hash,
            )
            .field("l2_to_l1_logs_root_hash", &self.l2_to_l1_logs_root_hash)
            .field("l2_da_validator", &self.l2_da_validator)
            .field("da_commitment", &self.da_commitment)
            .field("first_block_timestamp", &self.first_block_timestamp)
            .field("last_block_timestamp", &self.last_block_timestamp)
            .field("chain_id", &self.chain_id)
            .field("chain_address", &self.chain_address)
            // .field("operator_da_input", skipped to keep concise!)
            .field("upgrade_tx_hash", &self.upgrade_tx_hash)
            .finish()
    }
}
