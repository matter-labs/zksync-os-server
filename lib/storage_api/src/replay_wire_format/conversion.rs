use super::v1::ReplayWireFormatV1;
use super::v2::ReplayWireFormatV2;
use super::v3::ReplayWireFormatV3;
use super::v4::ReplayWireFormatV4;
use crate::ReplayRecord;
use alloy::eips::{Decodable2718, Encodable2718};
use alloy::primitives::{Address, U256};
use zksync_os_interface::types::{BlockContext, BlockHashes};
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
                // Old versions used to provide `gas_per_pubdata` instead of `pubdata_price` but it
                // was `0` for all existing blocks so it shouldn't matter.
                pubdata_price: gas_per_pubdata,
                native_price,
                coinbase: Address::new(coinbase.to_be_bytes()),
                gas_limit,
                pubdata_limit,
                mix_hash,
                execution_version: 1, // hardcoded for v1
                blob_fee: U256::ZERO,
            },
            starting_l1_priority_id,
            transactions: transactions.into_iter().map(|tx| tx.into()).collect(),
            previous_block_timestamp,
            node_version,
            block_output_hash,
        }
    }
}

impl From<ReplayWireFormatV2> for ReplayRecord {
    fn from(value: ReplayWireFormatV2) -> Self {
        let ReplayWireFormatV2 {
            block_context,
            starting_l1_priority_id,
            transactions,
            previous_block_timestamp,
            node_version,
            block_output_hash,
        } = value;
        let super::v2::BlockContext {
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
            execution_version,
        } = block_context;
        Self {
            block_context: BlockContext {
                chain_id,
                block_number,
                block_hashes: BlockHashes(block_hashes.0),
                timestamp,
                eip1559_basefee,
                // Old versions used to provide `gas_per_pubdata` instead of `pubdata_price` but it
                // was `0` for all existing blocks so it shouldn't matter.
                pubdata_price: gas_per_pubdata,
                native_price,
                coinbase,
                gas_limit,
                pubdata_limit,
                mix_hash,
                execution_version,
                blob_fee: U256::ZERO,
            },
            starting_l1_priority_id,
            transactions: transactions.into_iter().map(|tx| tx.into()).collect(),
            previous_block_timestamp,
            node_version,
            block_output_hash,
        }
    }
}

impl From<ReplayWireFormatV3> for ReplayRecord {
    fn from(value: ReplayWireFormatV3) -> Self {
        let ReplayWireFormatV3 {
            block_context,
            starting_l1_priority_id,
            transactions,
            previous_block_timestamp,
            node_version,
            block_output_hash,
        } = value;
        let super::v3::BlockContext {
            chain_id,
            block_number,
            block_hashes,
            timestamp,
            eip1559_basefee,
            pubdata_price,
            native_price,
            coinbase,
            gas_limit,
            pubdata_limit,
            mix_hash,
            execution_version,
        } = block_context;
        Self {
            block_context: BlockContext {
                chain_id,
                block_number,
                block_hashes: BlockHashes(block_hashes.0),
                timestamp,
                eip1559_basefee,
                pubdata_price,
                native_price,
                coinbase,
                gas_limit,
                pubdata_limit,
                mix_hash,
                execution_version,
                blob_fee: U256::ZERO,
            },
            starting_l1_priority_id,
            transactions: transactions.into_iter().map(|tx| tx.into()).collect(),
            previous_block_timestamp,
            node_version,
            block_output_hash,
        }
    }
}

impl From<ReplayWireFormatV4> for ReplayRecord {
    fn from(value: ReplayWireFormatV4) -> Self {
        let ReplayWireFormatV4 {
            block_context,
            starting_l1_priority_id,
            transactions,
            previous_block_timestamp,
            node_version,
            block_output_hash,
        } = value;
        let super::v4::BlockContext {
            chain_id,
            block_number,
            block_hashes,
            timestamp,
            eip1559_basefee,
            pubdata_price,
            native_price,
            coinbase,
            gas_limit,
            pubdata_limit,
            mix_hash,
            execution_version,
            blob_fee,
        } = block_context;
        Self {
            block_context: BlockContext {
                chain_id,
                block_number,
                block_hashes: BlockHashes(block_hashes.0),
                timestamp,
                eip1559_basefee,
                pubdata_price,
                native_price,
                coinbase,
                gas_limit,
                pubdata_limit,
                mix_hash,
                execution_version,
                blob_fee,
            },
            starting_l1_priority_id,
            transactions: transactions.into_iter().map(|tx| tx.into()).collect(),
            previous_block_timestamp,
            node_version,
            block_output_hash,
        }
    }
}

impl From<ReplayRecord> for ReplayWireFormatV4 {
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
            pubdata_price,
            native_price,
            coinbase,
            gas_limit,
            pubdata_limit,
            mix_hash,
            execution_version,
            blob_fee,
        } = block_context;
        Self {
            block_context: super::v4::BlockContext {
                chain_id,
                block_number,
                block_hashes: super::v4::BlockHashes(block_hashes.0),
                timestamp,
                eip1559_basefee,
                pubdata_price,
                native_price,
                coinbase,
                gas_limit,
                pubdata_limit,
                mix_hash,
                execution_version,
                blob_fee,
            },
            starting_l1_priority_id,
            transactions: transactions.into_iter().map(|tx| tx.into()).collect(),
            previous_block_timestamp,
            node_version,
            block_output_hash,
        }
    }
}

impl From<zksync_os_types::ZkTransaction> for super::v4::ZkTransactionWireFormat {
    fn from(value: zksync_os_types::ZkTransaction) -> Self {
        Self(value.inner.encoded_2718())
    }
}

impl From<super::v1::ZkTransactionWireFormat> for zksync_os_types::ZkTransaction {
    fn from(value: super::v1::ZkTransactionWireFormat) -> Self {
        ZkEnvelope::decode_2718(&mut &value.0[..])
            .unwrap()
            .try_into_recovered()
            .unwrap()
    }
}

impl From<super::v2::ZkTransactionWireFormat> for zksync_os_types::ZkTransaction {
    fn from(value: super::v2::ZkTransactionWireFormat) -> Self {
        ZkEnvelope::decode_2718(&mut &value.0[..])
            .unwrap()
            .try_into_recovered()
            .unwrap()
    }
}

impl From<super::v3::ZkTransactionWireFormat> for zksync_os_types::ZkTransaction {
    fn from(value: super::v3::ZkTransactionWireFormat) -> Self {
        ZkEnvelope::decode_2718(&mut &value.0[..])
            .unwrap()
            .try_into_recovered()
            .unwrap()
    }
}

impl From<super::v4::ZkTransactionWireFormat> for zksync_os_types::ZkTransaction {
    fn from(value: super::v4::ZkTransactionWireFormat) -> Self {
        ZkEnvelope::decode_2718(&mut &value.0[..])
            .unwrap()
            .try_into_recovered()
            .unwrap()
    }
}
