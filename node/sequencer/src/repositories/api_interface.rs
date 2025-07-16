use crate::repositories::transaction_receipt_repository::StoredTxData;
use crate::repositories::RepositoryManager;
use alloy::consensus::Block;
use alloy::primitives::TxHash;

pub trait ApiRepository {
    fn get_block_by_number(&self, number: u64) -> Option<Block<TxHash>>;

    fn get_stored_tx_by_hash(&self, tx_hash: TxHash) -> Option<StoredTxData>;

    fn get_canonized_block(&self) -> u64;
}

impl ApiRepository for RepositoryManager {
    fn get_block_by_number(&self, number: u64) -> Option<Block<TxHash>> {
        if let Some(res) = self.block_receipt_repository.get_by_number(number) {
            return Some(res);
        }

        self.db.get_block_by_number(number)
    }

    fn get_stored_tx_by_hash(&self, tx_hash: TxHash) -> Option<StoredTxData> {
        if let Some(res) = self
            .transaction_receipt_repository
            .get_stored_tx_by_hash(tx_hash)
        {
            return Some(res);
        }

        self.db.get_stored_tx_by_hash(tx_hash)
    }

    fn get_canonized_block(&self) -> u64 {
        *self.latest_block.borrow()
    }
}
