use crate::{
    ReplayRecord,
    replay_wire_format::{PreviousReplayWireFormat, ReplayWireFormat},
};
use zk_ee::system::metadata::BlockHashes;
use zk_os_forward_system::run::BlockContext;

impl From<ReplayWireFormat> for ReplayRecord {
    fn from(value: ReplayWireFormat) -> Self {
        let ReplayWireFormat {
            block_context,
            starting_l1_priority_id,
            transactions,
            previous_block_timestamp,
            node_version,
            block_output_hash,
        } = value;
        let crate::replay_wire_format::BlockContext {
            chain_id,
            block_number,
            block_hashes,
            timestamp,
            eip1559_basefee,
            gas_per_pubdata,
            native_price,
            coinbase,
            gas_limit,
            pubdata_limit,
            mix_hash,
        } = block_context;
        Self {
            block_context: BlockContext {
                chain_id,
                block_number,
                block_hashes: BlockHashes(block_hashes.0),
                timestamp,
                eip1559_basefee,
                gas_per_pubdata,
                native_price,
                coinbase,
                gas_limit,
                pubdata_limit,
                mix_hash,
            },
            starting_l1_priority_id,
            transactions,
            previous_block_timestamp,
            node_version,
            block_output_hash,
        }
    }
}

impl From<ReplayRecord> for ReplayWireFormat {
    fn from(value: ReplayRecord) -> Self {
        let ReplayRecord {
            block_context,
            starting_l1_priority_id,
            transactions,
            previous_block_timestamp,
            node_version,
            block_output_hash,
        } = value;
        let BlockContext {
            chain_id,
            block_number,
            block_hashes,
            timestamp,
            eip1559_basefee,
            gas_per_pubdata,
            native_price,
            coinbase,
            gas_limit,
            pubdata_limit,
            mix_hash,
        } = block_context;
        Self {
            block_context: crate::replay_wire_format::BlockContext {
                chain_id,
                block_number,
                block_hashes: crate::replay_wire_format::BlockHashes(block_hashes.0),
                timestamp,
                eip1559_basefee,
                gas_per_pubdata,
                native_price,
                coinbase,
                gas_limit,
                pubdata_limit,
                mix_hash,
            },
            starting_l1_priority_id,
            transactions,
            previous_block_timestamp,
            node_version,
            block_output_hash,
        }
    }
}

impl From<PreviousReplayWireFormat> for ReplayRecord {
    fn from(value: PreviousReplayWireFormat) -> Self {
        value.0.into()
    }
}
