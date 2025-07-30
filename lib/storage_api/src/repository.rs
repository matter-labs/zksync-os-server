use crate::model::{StoredTxData, TxMeta};
use alloy::consensus::Block;
use alloy::eips::{BlockHashOrNumber, BlockId, BlockNumberOrTag};
use alloy::primitives::{Address, BlockHash, BlockNumber, Sealed, TxHash, TxNonce};
use zksync_os_types::{ZkReceiptEnvelope, ZkTransaction};
use zksync_storage::rocksdb;

/// Sealed block (i.e. pre-computed hash) along with transaction hashes included in that block.
/// This is the structure stored in the repository and hence what is served in its API.
pub type RepositoryBlock = Sealed<Block<TxHash>>;

pub trait ApiRepository: Send + Sync {
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
}

/// Extension methods for `ApiRepository` implementations.
pub trait ApiRepositoryExt: ApiRepository {
    /// Get sealed block with transaction hashes by its hash OR number.
    fn get_block_by_hash_or_number(
        &self,
        hash_or_number: BlockHashOrNumber,
    ) -> RepositoryResult<Option<RepositoryBlock>> {
        match hash_or_number {
            BlockHashOrNumber::Hash(hash) => self.get_block_by_hash(hash),
            BlockHashOrNumber::Number(number) => self.get_block_by_number(number),
        }
    }

    /// Resolve block's hash OR number by its id. This method can be useful when caller does not
    /// care which of the block's hash or number to deal with and wants to perform as few look-up
    /// actions as possible.
    ///
    /// WARNING: Does not ensure that the returned block's hash or number actually exists
    fn resolve_block_hash_or_number(
        &self,
        block_id: BlockId,
    ) -> RepositoryResult<BlockHashOrNumber> {
        match block_id {
            BlockId::Hash(hash) => Ok(hash.block_hash.into()),
            BlockId::Number(BlockNumberOrTag::Pending) => Ok(self.get_latest_block().into()),
            BlockId::Number(BlockNumberOrTag::Latest) => Ok(self.get_latest_block().into()),
            BlockId::Number(BlockNumberOrTag::Safe) => Err(RepositoryError::SafeBlockNotSupported),
            BlockId::Number(BlockNumberOrTag::Finalized) => {
                Err(RepositoryError::FinalizedBlockNotSupported)
            }
            BlockId::Number(BlockNumberOrTag::Earliest) => {
                Err(RepositoryError::EarliestBlockNotSupported)
            }
            BlockId::Number(BlockNumberOrTag::Number(number)) => Ok(number.into()),
        }
    }

    /// Resolve block's number by its id.
    fn resolve_block_number(&self, block_id: BlockId) -> RepositoryResult<Option<BlockNumber>> {
        let block_hash_or_number = self.resolve_block_hash_or_number(block_id)?;
        match block_hash_or_number {
            // todo: should be possible to not load the entire block here
            BlockHashOrNumber::Hash(block_hash) => Ok(self
                .get_block_by_hash(block_hash)?
                .map(|header| header.number)),
            BlockHashOrNumber::Number(number) => Ok(Some(number)),
        }
    }

    /// Get sealed block with transaction hashes number by its id.
    fn get_block_by_id(&self, block_id: BlockId) -> RepositoryResult<Option<RepositoryBlock>> {
        // We presume that a reasonable number of historical blocks are being saved, so that
        // `Latest`/`Pending`/`Safe`/`Finalized` always resolve even if we don't take a look between
        // two actions below.
        let block_hash_or_number = self.resolve_block_hash_or_number(block_id)?;
        self.get_block_by_hash_or_number(block_hash_or_number)
    }
}

impl<R: ApiRepository> ApiRepositoryExt for R {}

/// Repository result type.
pub type RepositoryResult<Ok> = Result<Ok, RepositoryError>;

/// Error variants thrown by various repositories.
#[derive(Clone, Debug, thiserror::Error)]
pub enum RepositoryError {
    // todo: should resolve to last committed block here when available
    #[error("safe block tag is not supported yet")]
    SafeBlockNotSupported,
    // todo: should resolve to last executed block here when available
    #[error("finalized block tag is not supported yet")]
    FinalizedBlockNotSupported,
    // todo: should resolve to first non-compressed/non-pruned block? unclear, might depend on the method
    #[error("earliest block tag is not supported yet")]
    EarliestBlockNotSupported,

    #[error(transparent)]
    Rocksdb(#[from] rocksdb::Error),
    #[error(transparent)]
    Eip2718(#[from] alloy::eips::eip2718::Eip2718Error),
    #[error(transparent)]
    Rlp(#[from] alloy::rlp::Error),
}
