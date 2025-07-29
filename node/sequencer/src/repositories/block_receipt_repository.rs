use crate::repositories::api_interface::RepositoryBlock;
use alloy::primitives::{Address, B64, B256, BlockHash, BlockNumber, Bloom, Sealed, TxHash, U256};
use dashmap::DashMap;
use std::sync::Arc;

/// In-memory repository of the most recent N `BlockOutput`s, keyed by block number.
///
/// Inserts must happen in strictly ascending order.
///
#[derive(Clone, Debug, Default)]
pub struct BlockReceiptRepository {
    /// Map from block number → block hash.
    hash_index: Arc<DashMap<BlockNumber, BlockHash>>,
    /// Map from block hash → block.
    receipts: Arc<DashMap<BlockHash, Arc<Sealed<alloy::consensus::Block<TxHash>>>>>,
}

impl BlockReceiptRepository {
    /// Create a new repository.
    pub fn new() -> Self {
        BlockReceiptRepository::default()
    }

    /// Insert the `BlockOutput` for `block`.
    ///
    /// Must be called with `block == latest_block() + 1`.
    pub fn insert(&self, block: Arc<Sealed<alloy::consensus::Block<TxHash>>>) {
        let number = block.number;
        let hash = block.hash();
        self.receipts.insert(hash, block);
        self.hash_index.insert(number, hash);
    }

    /// Retrieve the block by its number, if present.
    pub fn get_by_number(&self, number: BlockNumber) -> Option<RepositoryBlock> {
        let hash = *self.hash_index.get(&number)?;
        self.get_by_hash(hash)
    }

    /// Retrieve the block by its hash, if present.
    pub fn get_by_hash(&self, hash: BlockHash) -> Option<RepositoryBlock> {
        self.receipts.get(&hash).map(|r| r.value().as_ref().clone())
    }

    pub fn remove_by_number(&self, number: BlockNumber) {
        if let Some((_, hash)) = self.hash_index.remove(&number) {
            self.receipts.remove(&hash);
        }
    }
}

pub fn alloy_header(
    header: &zk_os_forward_system::run::output::BlockHeader,
) -> alloy::consensus::Header {
    alloy::consensus::Header {
        parent_hash: B256::new(header.parent_hash.as_u8_array()),
        ommers_hash: B256::new(header.ommers_hash.as_u8_array()),
        beneficiary: Address::new(header.beneficiary.to_be_bytes()),
        state_root: B256::new(header.state_root.as_u8_array()),
        transactions_root: B256::new(header.transactions_root.as_u8_array()),
        receipts_root: B256::new(header.receipts_root.as_u8_array()),
        logs_bloom: Bloom::new(header.logs_bloom),
        difficulty: U256::from_be_bytes(header.difficulty.to_be_bytes::<32>()),
        number: header.number,
        gas_limit: header.gas_limit,
        gas_used: header.gas_used,
        timestamp: header.timestamp,
        extra_data: header.extra_data.to_vec().into(),
        mix_hash: B256::new(header.mix_hash.as_u8_array()),
        nonce: B64::new(header.nonce),
        base_fee_per_gas: Some(header.base_fee_per_gas),
        withdrawals_root: None,
        blob_gas_used: None,
        excess_blob_gas: None,
        parent_beacon_block_root: None,
        requests_hash: None,
    }
}
