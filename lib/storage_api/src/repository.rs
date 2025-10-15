use crate::model::{StoredTxData, TxMeta};
use alloy::consensus::Block;
use alloy::primitives::{Address, BlockHash, BlockNumber, Sealed, TxHash, TxNonce};
use std::fmt::Debug;
use zksync_os_interface::types::BlockOutput;
use zksync_os_rocksdb::rocksdb;
use zksync_os_types::{ZkReceiptEnvelope, ZkTransaction};

/// Sealed block (i.e. pre-computed hash) along with transaction hashes included in that block.
/// This is the structure stored in the repository and hence what is served in its API.
// todo: to be replaced with a ZKsync OS specific block structure with extra fields
pub type RepositoryBlock = Sealed<Block<TxHash>>;

/// Read-only view on repositories that can fetch data required for RPC but not for VM execution.
///
/// This includes auxiliary data such as block headers, raw transactions and transaction receipts.
pub trait ReadRepository: Debug + Send + Sync + 'static {
    /// Get sealed block with transaction hashes by its number.
    fn get_block_by_number(&self, number: BlockNumber)
    -> RepositoryResult<Option<RepositoryBlock>>;

    /// Get sealed block with transaction hashes by its hash.
    fn get_block_by_hash(&self, hash: BlockHash) -> RepositoryResult<Option<RepositoryBlock>>;

    /// Get RLP-2718 encoded transaction by its hash.
    fn get_raw_transaction(&self, hash: TxHash) -> RepositoryResult<Option<Vec<u8>>>;

    /// Get signed and recovered transaction by its hash.
    fn get_transaction(&self, hash: TxHash) -> RepositoryResult<Option<ZkTransaction>>;

    /// Get transaction's receipt by its hash.
    fn get_transaction_receipt(&self, hash: TxHash) -> RepositoryResult<Option<ZkReceiptEnvelope>>;

    /// Get transaction's metadata (additional fields in the context of a block that contains this
    /// transaction) by its hash.
    fn get_transaction_meta(&self, hash: TxHash) -> RepositoryResult<Option<TxMeta>>;

    /// Get transaction hash by its sender and nonce.
    fn get_transaction_hash_by_sender_nonce(
        &self,
        sender: Address,
        nonce: TxNonce,
    ) -> RepositoryResult<Option<TxHash>>;

    /// Get all transaction's data by its hash.
    fn get_stored_transaction(&self, hash: TxHash) -> RepositoryResult<Option<StoredTxData>>;

    /// Returns number of the last known block.
    fn get_latest_block(&self) -> u64;

    /// Returns earliest block number that is stored in the repository.
    fn get_earliest_block(&self) -> u64 {
        // We presume that blocks never get pruned, so genesis block is always our earliest
        // block.
        0
    }
}

pub trait WriteRepository: ReadRepository {
    fn populate(
        &self,
        block_output: BlockOutput,
        transactions: Vec<ZkTransaction>,
    ) -> impl Future<Output = ()> + Send;
}

/// Repository result type.
pub type RepositoryResult<Ok> = Result<Ok, RepositoryError>;

/// Error variants thrown by various repositories.
#[derive(Clone, Debug, thiserror::Error)]
pub enum RepositoryError {
    #[error(transparent)]
    Rocksdb(#[from] rocksdb::Error),
    #[error(transparent)]
    Eip2718(#[from] alloy::eips::eip2718::Eip2718Error),
    #[error(transparent)]
    Rlp(#[from] alloy::rlp::Error),
}
