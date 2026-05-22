use alloy::primitives::B256;
use std::sync::Arc;
use zksync_os_batch_types::compute_state_commitment;
use zksync_os_l1_watcher::BatchStateCommitmentSource;
use zksync_os_merkle_tree::{MerkleTree, RocksDBWrapper};
use zksync_os_storage::db::BlockReplayStorage;
use zksync_os_storage::lazy::RepositoryManager;
use zksync_os_storage_api::{ReadReplay, ReadRepository};

/// [`BatchStateCommitmentSource`] backed by the merkle tree, block storage,
/// and replay storage.
pub struct LocalBatchState {
    tree: Arc<MerkleTree<RocksDBWrapper>>,
    repositories: RepositoryManager,
    block_replay_storage: BlockReplayStorage,
}

impl LocalBatchState {
    pub fn new(
        tree: Arc<MerkleTree<RocksDBWrapper>>,
        repositories: RepositoryManager,
        block_replay_storage: BlockReplayStorage,
    ) -> Self {
        Self {
            tree,
            repositories,
            block_replay_storage,
        }
    }
}

impl BatchStateCommitmentSource for LocalBatchState {
    fn batch_state_commitment(&self, last_block_number: u64) -> anyhow::Result<Option<B256>> {
        let Some((root_hash, leaf_count)) = self.tree.root_info(last_block_number)? else {
            return Ok(None);
        };
        let Some(block) = self.repositories.get_block_by_number(last_block_number)? else {
            return Ok(None);
        };
        let Some(record) = self
            .block_replay_storage
            .get_replay_record(last_block_number)
        else {
            return Ok(None);
        };
        Ok(Some(compute_state_commitment(
            root_hash,
            leaf_count,
            last_block_number,
            record.block_context.timestamp,
            block.hash(),
            &record.block_context.block_hashes,
        )))
    }
}
