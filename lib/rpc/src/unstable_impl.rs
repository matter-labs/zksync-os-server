use crate::ReadRpcStorage;
use crate::result::ToRpcResult;
use alloy::primitives::{B256, BlockNumber, TxHash};
use async_trait::async_trait;
use jsonrpsee::core::RpcResult;
use zksync_os_mini_merkle_tree::MiniMerkleTree;
use zksync_os_rpc_api::unstable::UnstableApiServer;
use zksync_os_storage_api::{PersistedBatch, RepositoryError};
use zksync_os_types::L2_TO_L1_TREE_SIZE;

pub struct UnstableNamespace<RpcStorage> {
    storage: RpcStorage,
}

impl<RpcStorage> UnstableNamespace<RpcStorage> {
    pub fn new(storage: RpcStorage) -> Self {
        Self { storage }
    }
}

impl<RpcStorage: ReadRpcStorage> UnstableNamespace<RpcStorage> {
    fn get_batch_by_block_number_impl(&self, block_number: u64) -> UnstableResult<PersistedBatch> {
        self.storage
            .batch()
            .get_batch_by_block_number(block_number)?
            .ok_or(UnstableError::BatchNotAvailableYet)
    }

    fn get_local_root_impl(&self, batch_number: u64) -> UnstableResult<B256> {
        let batch = self
            .storage
            .batch()
            .get_batch_by_number(batch_number)?
            .ok_or(UnstableError::BatchNotAvailableYet)?;

        let mut merkle_tree_leaves = vec![];
        for block in batch.block_range.clone() {
            let Some(block) = self.storage.repository().get_block_by_number(block)? else {
                return Err(UnstableError::BlockNotAvailable(block));
            };
            for block_tx_hash in block.unseal().body.transactions {
                let Some(receipt) = self
                    .storage
                    .repository()
                    .get_transaction_receipt(block_tx_hash)?
                else {
                    return Err(UnstableError::TxNotAvailable(block_tx_hash));
                };
                let l2_to_l1_logs = receipt.into_l2_to_l1_logs();
                for l2_to_l1_log in l2_to_l1_logs {
                    merkle_tree_leaves.push(l2_to_l1_log.encode());
                }
            }
        }

        let local_root =
            MiniMerkleTree::new(merkle_tree_leaves.into_iter(), Some(L2_TO_L1_TREE_SIZE))
                .merkle_root();

        Ok(local_root)
    }
}

#[async_trait]
impl<RpcStorage: ReadRpcStorage> UnstableApiServer for UnstableNamespace<RpcStorage> {
    async fn get_batch_by_block_number(&self, block_number: u64) -> RpcResult<PersistedBatch> {
        self.get_batch_by_block_number_impl(block_number)
            .to_rpc_result()
    }

    async fn get_local_root(&self, batch_number: u64) -> RpcResult<B256> {
        self.get_local_root_impl(batch_number).to_rpc_result()
    }
}

/// `unstable` namespace result type.
pub type UnstableResult<Ok> = Result<Ok, UnstableError>;

/// General `unstable` namespace errors
#[derive(Debug, thiserror::Error)]
pub enum UnstableError {
    #[error(
        "L1 batch containing the transaction has not been finalized or indexed by this node yet"
    )]
    BatchNotAvailableYet,
    #[error(transparent)]
    Batch(#[from] anyhow::Error),
    /// Historical block could not be found on this node (e.g., pruned).
    #[error("historical block {0} is not available")]
    BlockNotAvailable(BlockNumber),
    /// Historical transaction could not be found on this node (e.g., pruned).
    #[error("historical transaction {0} is not available")]
    TxNotAvailable(TxHash),
    #[error(transparent)]
    Repository(#[from] RepositoryError),
}
