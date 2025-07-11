use alloy::consensus::{Block, BlockBody, Header};
use alloy::primitives::{Address, Bloom, TxHash, B256, B64, U256};
use dashmap::DashMap;
use std::sync::Arc;
use zk_os_forward_system::run::output::BlockHeader;

/// In-memory repository of the most recent N `BatchOutput`s, keyed by block number.
///
/// Inserts must happen in strictly ascending order.
///
#[derive(Clone, Debug, Default)]
pub struct BlockReceiptRepository {
    /// Map from block number → block.
    receipts: Arc<DashMap<u64, Block<TxHash>>>,
}

impl BlockReceiptRepository {
    /// Create a new repository.
    pub fn new() -> Self {
        BlockReceiptRepository::default()
    }

    /// Insert the `BatchOutput` for `block`.
    ///
    /// Must be called with `block == latest_block() + 1`.
    pub fn insert(&self, header: &BlockHeader, tx_hashes: Vec<TxHash>) {
        let number = header.number;
        let block = Block {
            header: alloy_header(header),
            body: BlockBody {
                transactions: tx_hashes.clone(),
                ommers: vec![],
                withdrawals: None,
            },
        };
        // Store the new receipt
        self.receipts.insert(number, block);
    }

    /// Retrieve the block for `number`, if present.
    pub fn get_by_number(&self, number: u64) -> Option<Block<TxHash>> {
        self.receipts.get(&number).map(|r| r.value().clone())
    }

    pub fn remove_by_number(&self, number: u64) {
        self.receipts.remove(&number);
    }
}

fn alloy_header(header: &BlockHeader) -> Header {
    Header {
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
