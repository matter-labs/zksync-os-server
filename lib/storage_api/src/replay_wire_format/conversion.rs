use super::v1::{ReplayWireFormat, ZkTransactionWireFormat};
use crate::ReplayRecord;
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
        Self {
            signer: value.signer(),
            inner: value.inner.into_inner().into(),
        }
    }
}

impl From<ZkTransactionWireFormat> for zksync_os_types::ZkTransaction {
    fn from(value: ZkTransactionWireFormat) -> Self {
        Self {
            inner: alloy::consensus::transaction::Recovered::new_unchecked(
                value.inner.into(),
                value.signer,
            ),
        }
    }
}

impl From<zksync_os_types::ZkEnvelope> for super::v1::ZkEnvelope {
    fn from(value: zksync_os_types::ZkEnvelope) -> Self {
        match value {
            zksync_os_types::ZkEnvelope::Upgrade(envelope) => {
                super::v1::ZkEnvelope::Upgrade(envelope)
            }
            zksync_os_types::ZkEnvelope::L1(envelope) => super::v1::ZkEnvelope::L1(envelope),
            zksync_os_types::ZkEnvelope::L2(envelope) => super::v1::ZkEnvelope::L2(envelope.into()),
        }
    }
}

impl From<super::v1::ZkEnvelope> for zksync_os_types::ZkEnvelope {
    fn from(value: super::v1::ZkEnvelope) -> Self {
        match value {
            super::v1::ZkEnvelope::Upgrade(envelope) => {
                zksync_os_types::ZkEnvelope::Upgrade(envelope)
            }
            super::v1::ZkEnvelope::L1(envelope) => zksync_os_types::ZkEnvelope::L1(envelope),
            super::v1::ZkEnvelope::L2(envelope) => zksync_os_types::ZkEnvelope::L2(envelope.into()),
        }
    }
}
