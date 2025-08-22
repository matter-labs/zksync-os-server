use alloy::eips::{BlockHashOrNumber, BlockId, BlockNumberOrTag};
use alloy::primitives::BlockNumber;
use zk_os_forward_system::run::{PreimageSource, ReadStorageTree};
use zksync_os_storage_api::notifications::SubscribeToBlocks;
use zksync_os_storage_api::{
    ReadBatch, ReadFinality, ReadReplay, ReadRepository, ReadStateHistory, RepositoryBlock,
    RepositoryResult, StateResult,
};

pub trait ReadRpcStorage: ReadStateHistory + Clone {
    fn repository(&self) -> &dyn ReadRepository;
    fn block_subscriptions(&self) -> &dyn SubscribeToBlocks;
    fn replay_storage(&self) -> &dyn ReadReplay;
    fn finality(&self) -> &dyn ReadFinality;
    fn batch(&self) -> &dyn ReadBatch;

    /// Get sealed block with transaction hashes by its hash OR number.
    fn get_block_by_hash_or_number(
        &self,
        hash_or_number: BlockHashOrNumber,
    ) -> RepositoryResult<Option<RepositoryBlock>> {
        match hash_or_number {
            BlockHashOrNumber::Hash(hash) => self.repository().get_block_by_hash(hash),
            BlockHashOrNumber::Number(number) => self.repository().get_block_by_number(number),
        }
    }

    /// Resolve block's hash OR number by its id. This method can be useful when caller does not
    /// care which of the block's hash or number to deal with and wants to perform as few look-up
    /// actions as possible.
    ///
    /// WARNING: Does not ensure that the returned block's hash or number actually exists
    fn resolve_block_hash_or_number(&self, block_id: BlockId) -> BlockHashOrNumber {
        match block_id {
            BlockId::Hash(hash) => hash.block_hash.into(),
            BlockId::Number(BlockNumberOrTag::Pending) => {
                self.repository().get_latest_block().into()
            }
            BlockId::Number(BlockNumberOrTag::Latest) => {
                self.repository().get_latest_block().into()
            }
            BlockId::Number(BlockNumberOrTag::Safe) => self
                .finality()
                .get_finality_status()
                .last_committed_block
                .into(),
            BlockId::Number(BlockNumberOrTag::Finalized) => self
                .finality()
                .get_finality_status()
                .last_executed_block
                .into(),
            BlockId::Number(BlockNumberOrTag::Earliest) => {
                self.repository().get_earliest_block().into()
            }
            BlockId::Number(BlockNumberOrTag::Number(number)) => number.into(),
        }
    }

    /// Resolve block's number by its id.
    fn resolve_block_number(&self, block_id: BlockId) -> RepositoryResult<Option<BlockNumber>> {
        let block_hash_or_number = self.resolve_block_hash_or_number(block_id);
        match block_hash_or_number {
            // todo: should be possible to not load the entire block here
            BlockHashOrNumber::Hash(block_hash) => Ok(self
                .repository()
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
        let block_hash_or_number = self.resolve_block_hash_or_number(block_id);
        self.get_block_by_hash_or_number(block_hash_or_number)
    }
}

#[derive(Clone)]
pub struct RpcStorage<Repository, Replay, Finality, Batch, StateHistory> {
    repository: Repository,
    replay_storage: Replay,
    finality: Finality,
    batch: Batch,
    state: StateHistory,
}

impl<Repository, Replay, Finality, Batch, StateHistory>
    RpcStorage<Repository, Replay, Finality, Batch, StateHistory>
{
    pub fn new(
        repository: Repository,
        replay_storage: Replay,
        finality: Finality,
        batch: Batch,
        state: StateHistory,
    ) -> Self {
        Self {
            repository,
            replay_storage,
            finality,
            batch,
            state,
        }
    }
}

impl<
    Repository: ReadRepository + SubscribeToBlocks + Clone,
    Replay: ReadReplay + Clone,
    Finality: ReadFinality + Clone,
    Batch: ReadBatch + Clone,
    StateHistory: ReadStateHistory + Clone,
> ReadRpcStorage for RpcStorage<Repository, Replay, Finality, Batch, StateHistory>
{
    fn repository(&self) -> &dyn ReadRepository {
        &self.repository
    }

    fn block_subscriptions(&self) -> &dyn SubscribeToBlocks {
        &self.repository
    }

    fn replay_storage(&self) -> &dyn ReadReplay {
        &self.replay_storage
    }

    fn finality(&self) -> &dyn ReadFinality {
        &self.finality
    }

    fn batch(&self) -> &dyn ReadBatch {
        &self.batch
    }
}

impl<
    Repository: ReadRepository + SubscribeToBlocks + Clone,
    Replay: ReadReplay + Clone,
    Finality: ReadFinality + Clone,
    Batch: ReadBatch + Clone,
    StateHistory: ReadStateHistory + Clone,
> ReadStateHistory for RpcStorage<Repository, Replay, Finality, Batch, StateHistory>
{
    fn state_view_at(
        &self,
        block_number: BlockNumber,
    ) -> StateResult<impl ReadStorageTree + PreimageSource + Clone> {
        self.state.state_view_at(block_number)
    }
}
