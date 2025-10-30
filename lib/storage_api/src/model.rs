use alloy::primitives::{Address, B256};
use alloy::rlp::{RlpDecodable, RlpEncodable};
use serde::{Deserialize, Serialize};
use zksync_os_interface::types::BlockContext;
use zksync_os_types::{L1TxSerialId, ZkEnvelope, ZkReceiptEnvelope, ZkTransaction};

#[derive(Debug, Clone, RlpEncodable, RlpDecodable)]
#[rlp(trailing)]
pub struct TxMeta {
    pub block_hash: B256,
    pub block_number: u64,
    pub block_timestamp: u64,
    pub tx_index_in_block: u64,
    pub effective_gas_price: u128,
    pub number_of_logs_before_this_tx: u64,
    pub gas_used: u64,
    pub contract_address: Option<Address>,
}

#[derive(Debug, Clone)]
pub struct StoredTxData {
    pub tx: ZkTransaction,
    pub receipt: ZkReceiptEnvelope,
    pub meta: TxMeta,
}

/// Full data needed to replay a block - assuming storage is already in the correct state.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReplayRecord {
    pub block_context: BlockContext,
    /// L1 transaction serial id (0-based) expected at the beginning of this block.
    /// If `l1_transactions` is non-empty, equals to the first tx id in this block
    /// otherwise, `last_processed_l1_tx_id` equals to the previous block's value
    pub starting_l1_priority_id: L1TxSerialId,
    pub transactions: Vec<ZkTransaction>,
    /// The field is used to generate the prover input for the block in ProverInputGenerator.
    /// Will be moved to the BlockContext at some point
    pub previous_block_timestamp: u64,
    /// Version of the node that created this replay record.
    pub node_version: semver::Version,
    /// Hash of the block output.
    pub block_output_hash: B256,
}

impl ReplayRecord {
    pub fn new(
        block_context: BlockContext,
        starting_l1_priority_id: L1TxSerialId,
        transactions: Vec<ZkTransaction>,
        previous_block_timestamp: u64,
        node_version: semver::Version,
        block_output_hash: B256,
    ) -> Self {
        let first_l1_tx_priority_id = transactions.iter().find_map(|tx| match tx.envelope() {
            ZkEnvelope::L1(l1_tx) => Some(l1_tx.priority_id()),
            ZkEnvelope::L2(_) => None,
            ZkEnvelope::Upgrade(_) => None,
        });
        if let Some(first_l1_tx_priority_id) = first_l1_tx_priority_id {
            assert_eq!(
                first_l1_tx_priority_id, starting_l1_priority_id,
                "First L1 tx priority id must match next_l1_priority_id"
            );
        }
        Self {
            block_context,
            starting_l1_priority_id,
            transactions,
            previous_block_timestamp,
            node_version,
            block_output_hash,
        }
    }
}

/// Chain's L1 finality status. Does not track last proved block as there is no need for it (yet).
#[derive(Clone, Debug)]
pub struct FinalityStatus {
    pub last_committed_block: u64,
    pub last_committed_batch: u64,
    pub last_executed_block: u64,
    pub last_executed_batch: u64,
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{Address, U256};
    use serde::{Deserialize, Serialize};
    use zksync_os_interface::types::{BlockContext, BlockHashes};

    #[test]
    fn block_context_bincode_compatibility() {
        // Test that renaming the `gas_per_pubdata` field did not break bincode compatibility.
        #[derive(Clone, Copy, Debug, PartialEq, Default, Serialize, Deserialize)]
        pub struct OldBlockContext {
            // Chain id is temporarily also added here (so that it can be easily passed from the oracle)
            // long term, we have to decide whether we want to keep it here, or add a separate oracle
            // type that would return some 'chain' specific metadata (as this class is supposed to hold block metadata only).
            pub chain_id: u64,
            pub block_number: u64,
            pub block_hashes: BlockHashes,
            pub timestamp: u64,
            pub eip1559_basefee: U256,
            pub gas_per_pubdata: U256,
            pub native_price: U256,
            pub coinbase: Address,
            pub gas_limit: u64,
            pub pubdata_limit: u64,
            /// Source of randomness, currently holds the value
            /// of prevRandao.
            pub mix_hash: U256,
            /// Version of the ZKsync OS and its config to be used for this block.
            pub execution_version: u32,
        }

        let old_block_context = OldBlockContext {
            chain_id: 270,
            block_number: 134,
            block_hashes: Default::default(),
            timestamp: 74823,
            eip1559_basefee: U256::from(72138974),
            gas_per_pubdata: U256::from(287472893),
            native_price: U256::from(289773424),
            coinbase: Default::default(),
            gas_limit: 897234273,
            pubdata_limit: 8392472,
            mix_hash: Default::default(),
            execution_version: 17,
        };
        let encoded =
            bincode::serde::encode_to_vec(old_block_context, bincode::config::standard()).unwrap();
        let (new_block_context, _) = bincode::serde::decode_from_slice::<BlockContext, _>(
            &encoded,
            bincode::config::standard(),
        )
        .unwrap();
        assert_eq!(
            new_block_context.pubdata_price,
            old_block_context.gas_per_pubdata
        );
        assert_eq!(new_block_context.chain_id, old_block_context.chain_id);
        assert_eq!(
            new_block_context.execution_version,
            old_block_context.execution_version
        );
    }
}
