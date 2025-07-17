use crate::repositories::bytecode_property_respository::BytecodeRepository;
use crate::repositories::db::DbError;
use crate::repositories::transaction_receipt_repository::{StoredTxData, TxMeta};
use crate::repositories::{AccountPropertyRepository, RepositoryManager};
use alloy::consensus::{Block, ReceiptEnvelope};
use alloy::eips::{BlockHashOrNumber, BlockId, BlockNumberOrTag, Encodable2718};
use alloy::primitives::{Address, BlockHash, BlockNumber, Sealed, TxHash, TxNonce};
use zksync_os_types::ZkTransaction;

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
    fn get_transaction_receipt(&self, hash: TxHash) -> RepositoryResult<Option<ReceiptEnvelope>>;

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

    /// Returns number of the last canonized block.
    fn get_canonized_block(&self) -> u64;

    // todo(#36): temporary, remove from here
    fn account_property_repository(&self) -> &AccountPropertyRepository;

    // todo(#36): temporary, remove from here
    fn bytecode_repository(&self) -> &BytecodeRepository;
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
            BlockId::Number(BlockNumberOrTag::Pending) => Ok(self.get_canonized_block().into()),
            BlockId::Number(BlockNumberOrTag::Latest) => Ok(self.get_canonized_block().into()),
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

impl ApiRepository for RepositoryManager {
    fn get_block_by_number(
        &self,
        number: BlockNumber,
    ) -> RepositoryResult<Option<RepositoryBlock>> {
        if let Some(block) = self.block_receipt_repository.get_by_number(number) {
            return Ok(Some(block));
        }

        Ok(self.db.get_block_by_number(number)?)
    }

    fn get_block_by_hash(&self, hash: BlockHash) -> RepositoryResult<Option<RepositoryBlock>> {
        if let Some(block) = self.block_receipt_repository.get_by_hash(hash) {
            return Ok(Some(block));
        }

        Ok(self.db.get_block_by_hash(hash)?)
    }

    fn get_raw_transaction(&self, hash: TxHash) -> RepositoryResult<Option<Vec<u8>>> {
        // todo: implement efficiently
        Ok(self
            .get_stored_transaction(hash)?
            .map(|stored_tx| stored_tx.tx.into_envelope().encoded_2718()))
    }

    fn get_transaction(&self, hash: TxHash) -> RepositoryResult<Option<ZkTransaction>> {
        // todo: implement efficiently
        Ok(self
            .get_stored_transaction(hash)?
            .map(|stored_tx| stored_tx.tx))
    }

    fn get_transaction_receipt(&self, hash: TxHash) -> RepositoryResult<Option<ReceiptEnvelope>> {
        // todo: implement efficiently
        Ok(self
            .get_stored_transaction(hash)?
            .map(|stored_tx| stored_tx.receipt))
    }

    fn get_transaction_meta(&self, hash: TxHash) -> RepositoryResult<Option<TxMeta>> {
        // todo: implement efficiently
        Ok(self
            .get_stored_transaction(hash)?
            .map(|stored_tx| stored_tx.meta))
    }

    fn get_transaction_hash_by_sender_nonce(
        &self,
        sender: Address,
        nonce: TxNonce,
    ) -> RepositoryResult<Option<TxHash>> {
        if let Some(tx_hash) = self
            .transaction_receipt_repository
            .get_transaction_hash_by_sender_nonce(sender, nonce)
        {
            return Ok(Some(tx_hash));
        }

        Ok(self
            .db
            .get_transaction_hash_by_sender_nonce(sender, nonce)?)
    }

    fn get_stored_transaction(&self, hash: TxHash) -> RepositoryResult<Option<StoredTxData>> {
        if let Some(res) = self
            .transaction_receipt_repository
            .get_stored_tx_by_hash(hash)
        {
            return Ok(Some(res));
        }

        Ok(self.db.get_stored_tx_by_hash(hash)?)
    }

    fn get_canonized_block(&self) -> u64 {
        *self.latest_block.borrow()
    }

    fn account_property_repository(&self) -> &AccountPropertyRepository {
        &self.account_property_repository
    }

    fn bytecode_repository(&self) -> &BytecodeRepository {
        &self.bytecode_repository
    }
}

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
    DbError(#[from] DbError),
}
