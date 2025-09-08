use super::v1::{ReplayWireFormatV1, ZkTransactionWireFormat};
use crate::ReplayRecord;
use alloy::eips::{Decodable2718, Encodable2718};
use zksync_os_interface::common_types::{BlockContext, BlockHashes};
use zksync_os_types::ZkEnvelope;

impl From<ReplayWireFormatV1> for ReplayRecord {
    fn from(value: ReplayWireFormatV1) -> Self {
        let ReplayWireFormatV1 {
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

impl From<ReplayRecord> for ReplayWireFormatV1 {
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
