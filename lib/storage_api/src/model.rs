use alloy::primitives::{Address, B256, U256};
use alloy::rlp::{RlpDecodable, RlpEncodable};
use serde::{Deserialize, Serialize};
use zksync_os_batch_types::BlockMerkleTreeData;
use zksync_os_interface::traits::AnyBlockContext;
use zksync_os_pipeline::HasBlockRangeEnd;
use zksync_os_types::{
    BlockOutput, BlockStartCursors, ProtocolSemanticVersion, ZkEnvelope, ZkReceiptEnvelope,
    ZkTransaction,
};

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
    pub transactions: Vec<ZkTransaction>,
    /// The field is used to generate the prover input for the block in ProverInputGenerator.
    /// Will be moved to the BlockContext at some point
    pub previous_block_timestamp: u64,
    /// Version of the node that created this replay record.
    /// NOTE: Excluded from equality check, different node versions can produce identical blocks.
    pub node_version: semver::Version,
    /// Version of the protocol that was used to create this replay record.
    pub protocol_version: ProtocolSemanticVersion,
    /// Extension of traditional block hash (see hash_block_output)
    /// Used to confirm that we executed the replay correctly
    /// We need this because our header is missing a few important fields
    // TODO: We may want to add more information about block_output_hash (currently tracks only output, but different input can result in same output)
    pub block_output_hash: B256,
    /// Forced preimages to be included before the block execution.
    pub force_preimages: Vec<(B256, Vec<u8>)>,
    /// Cursors at the start of this block. Tracks where each L1 data source
    /// (priority txs, interop events, migrations, fee updates) left off.
    /// Flattened for serde backwards-compatibility with the old flat field layout.
    #[serde(flatten)]
    pub starting_cursors: BlockStartCursors,
}

impl PartialEq for ReplayRecord {
    fn eq(&self, other: &Self) -> bool {
        // Same as #[derive(PartialEq)], but without `node_version`.
        // During replay, we care about block identity, node_version is binary metadata.
        self.block_context == other.block_context
            && self.transactions == other.transactions
            && self.previous_block_timestamp == other.previous_block_timestamp
            && self.protocol_version == other.protocol_version
            && self.block_output_hash == other.block_output_hash
            && self.force_preimages == other.force_preimages
            && self.starting_cursors == other.starting_cursors
    }
}

impl ReplayRecord {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        block_context: BlockContext,
        transactions: Vec<ZkTransaction>,
        previous_block_timestamp: u64,
        node_version: semver::Version,
        protocol_version: ProtocolSemanticVersion,
        block_output_hash: B256,
        force_preimages: Vec<(B256, Vec<u8>)>,
        starting_cursors: BlockStartCursors,
    ) -> Self {
        let first_l1_tx_priority_id = transactions.iter().find_map(|tx| match tx.envelope() {
            ZkEnvelope::L1(l1_tx) => Some(l1_tx.priority_id()),
            _ => None,
        });
        if let Some(first_l1_tx_priority_id) = first_l1_tx_priority_id {
            assert_eq!(
                first_l1_tx_priority_id, starting_cursors.l1_priority_id,
                "First L1 tx priority id must match next_l1_priority_id"
            );
        }

        Self {
            block_context,
            transactions,
            previous_block_timestamp,
            node_version,
            protocol_version,
            block_output_hash,
            force_preimages,
            starting_cursors,
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
    pub last_finalized_executed_block: u64,
    pub last_finalized_executed_batch: u64,
}

/// Message flowing from `TreeManager` → `ProverInputGenerator` / `BatchVerificationResponder`.
pub struct TreeBlock {
    pub output: BlockOutput,
    pub record: ReplayRecord,
    pub tree: BlockMerkleTreeData,
}

impl HasBlockRangeEnd for TreeBlock {
    fn block_number(&self) -> u64 {
        self.record.block_context.block_number
    }
    fn block_timestamp(&self) -> Option<u64> {
        Some(self.record.block_context.timestamp)
    }
}

impl HasBlockRangeEnd for ReplayRecord {
    fn block_number(&self) -> u64 {
        self.block_context.block_number
    }
    fn block_timestamp(&self) -> Option<u64> {
        Some(self.block_context.timestamp)
    }
}

/// Be careful when changing this struct as making non-backwards-compatible changes will make old
/// storage non-loadable.
#[derive(Clone, Copy, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct BlockContext {
    // Chain id is temporarily also added here (so that it can be easily passed from the oracle)
    // long term, we have to decide whether we want to keep it here, or add a separate oracle
    // type that would return some 'chain' specific metadata (as this class is supposed to hold block metadata only).
    pub chain_id: u64,
    pub block_number: u64,
    pub block_hashes: BlockHashes,
    pub timestamp: u64,
    pub eip1559_basefee: U256,
    pub pubdata_price: U256,
    pub native_price: U256,
    pub coinbase: Address,
    pub gas_limit: u64,
    pub pubdata_limit: u64,
    /// Source of randomness, currently holds the value of prevRandao.
    pub mix_hash: U256,
    /// Version of the ZKsync OS and its config to be used for this block.
    pub execution_version: u32,
    pub blob_fee: U256,
}

/// Array of previous block hashes.
/// Hash for block number N will be at index [256 - (current_block_number - N)]
/// (most recent will be at the end) if N is one of the most recent
/// 256 blocks.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BlockHashes(pub [U256; 256]);

impl BlockHashes {
    pub fn push(self, block_hash: B256) -> Self {
        Self(
            self.0
                .into_iter()
                .skip(1)
                .chain([U256::from_be_bytes(block_hash.0)])
                .collect::<Vec<_>>()
                .try_into()
                .unwrap(),
        )
    }
}

impl Default for BlockHashes {
    fn default() -> Self {
        Self([U256::ZERO; 256])
    }
}

impl serde::Serialize for BlockHashes {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.0.to_vec().serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for BlockHashes {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let vec: Vec<U256> = Vec::deserialize(deserializer)?;
        let array: [U256; 256] = vec
            .try_into()
            .map_err(|_| serde::de::Error::custom("Expected array of length 256"))?;
        Ok(Self(array))
    }
}

impl AnyBlockContext for BlockContext {
    fn chain_id(&self) -> u64 {
        self.chain_id
    }

    fn block_number(&self) -> u64 {
        self.block_number
    }

    fn block_hashes(&self) -> &[U256; 256] {
        &self.block_hashes.0
    }

    fn timestamp(&self) -> u64 {
        self.timestamp
    }

    fn eip1559_basefee(&self) -> U256 {
        self.eip1559_basefee
    }

    fn pubdata_price(&self) -> U256 {
        self.pubdata_price
    }

    fn native_price(&self) -> U256 {
        self.native_price
    }

    fn coinbase(&self) -> Address {
        self.coinbase
    }

    fn gas_limit(&self) -> u64 {
        self.gas_limit
    }

    fn pubdata_limit(&self) -> u64 {
        self.pubdata_limit
    }

    fn mix_hash(&self) -> U256 {
        self.mix_hash
    }

    fn blob_fee(&self) -> U256 {
        self.blob_fee
    }

    fn is_gateway(&self) -> bool {
        // todo: source from a new optional field?
        false
    }
}
