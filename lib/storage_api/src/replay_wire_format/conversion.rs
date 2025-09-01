use super::v1::{ReplayWireFormat, ZkTransactionWireFormat};
use crate::ReplayRecord;
use alloy::eips::{Decodable2718, Encodable2718};
use zk_ee::system::metadata::BlockHashes;
use zk_os_forward_system::run::BlockContext;
use zksync_os_types::ZkEnvelope;

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
        let super::v1::BlockContext {
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
            transactions: transactions.into_iter().map(|tx| tx.into()).collect(),
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
            block_context: super::v1::BlockContext {
                chain_id,
                block_number,
                block_hashes: super::v1::BlockHashes(block_hashes.0),
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
            transactions: transactions.into_iter().map(|tx| tx.into()).collect(),
            previous_block_timestamp,
            node_version,
            block_output_hash,
        }
    }
}

impl From<zksync_os_types::ZkTransaction> for ZkTransactionWireFormat {
    fn from(value: zksync_os_types::ZkTransaction) -> Self {
        Self(value.inner.encoded_2718())
    }
}

impl From<ZkTransactionWireFormat> for zksync_os_types::ZkTransaction {
    fn from(value: ZkTransactionWireFormat) -> Self {
        ZkEnvelope::decode_2718(&mut &value.0[..])
            .unwrap()
            .try_into_recovered()
            .unwrap()
    }
}
