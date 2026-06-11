use crate::ReadRpcStorage;
use crate::result::ToRpcResult;
use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::primitives::TxHash;
use jsonrpsee::core::RpcResult;
use zksync_os_rpc_api::finality::{
    FinalityResponse, FinalityStage, NodeFinalityStatus, ZksFinalityApiServer,
};
use zksync_os_storage_api::RepositoryError;

pub struct FinalityNamespace<RpcStorage> {
    storage: RpcStorage,
}

impl<RpcStorage> FinalityNamespace<RpcStorage> {
    pub fn new(storage: RpcStorage) -> Self {
        Self { storage }
    }
}

/// Classifies a block or batch number against the committed, executed and finalized frontiers.
fn finality_stage(number: u64, committed: u64, executed: u64, finalized: u64) -> FinalityStage {
    if number <= finalized {
        FinalityStage::Finalized
    } else if number <= executed {
        FinalityStage::Executed
    } else if number <= committed {
        FinalityStage::Committed
    } else {
        FinalityStage::Pending
    }
}

impl<RpcStorage: ReadRpcStorage> FinalityNamespace<RpcStorage> {
    /// Builds a [`FinalityResponse`] for a block known to exist, enriching it with the containing
    /// batch number once that batch is executed.
    fn block_finality(&self, block_number: u64) -> FinalityResult<FinalityResponse> {
        let finality = self.storage.finality().get_finality_status();
        let batch = self
            .storage
            .batch()
            .get_batch_by_block_number(block_number)?;
        Ok(FinalityResponse {
            stage: finality_stage(
                block_number,
                finality.last_committed_block,
                finality.last_executed_block,
                finality.last_finalized_executed_block,
            ),
            block_number: Some(block_number),
            batch_number: batch.map(|b| b.number()),
        })
    }

    fn get_block_finality_impl(
        &self,
        block: BlockNumberOrTag,
    ) -> FinalityResult<Option<FinalityResponse>> {
        let Some(block_number) = self.storage.resolve_block_number(BlockId::Number(block))? else {
            return Ok(None);
        };
        // `resolve_block_number` does not check existence for an explicit number, so bound it by the
        // latest sealed block; tags always resolve to an existing block.
        if block_number > self.storage.repository().get_latest_block() {
            return Ok(None);
        }
        Ok(Some(self.block_finality(block_number)?))
    }

    fn get_transaction_finality_impl(
        &self,
        tx_hash: TxHash,
    ) -> FinalityResult<Option<FinalityResponse>> {
        let Some(meta) = self.storage.repository().get_transaction_meta(tx_hash)? else {
            return Ok(None);
        };
        Ok(Some(self.block_finality(meta.block_number)?))
    }

    fn get_batch_finality_impl(&self, batch_number: u64) -> FinalityResult<FinalityResponse> {
        let finality = self.storage.finality().get_finality_status();
        // The block range is only persisted once the batch is executed.
        let batch = self.storage.batch().get_batch_by_number(batch_number)?;
        Ok(FinalityResponse {
            stage: finality_stage(
                batch_number,
                finality.last_committed_batch,
                finality.last_executed_batch,
                finality.last_finalized_executed_batch,
            ),
            block_number: batch.map(|b| b.last_block_number()),
            batch_number: Some(batch_number),
        })
    }
}

impl<RpcStorage: ReadRpcStorage> ZksFinalityApiServer for FinalityNamespace<RpcStorage> {
    fn get_block_finality(&self, block: BlockNumberOrTag) -> RpcResult<Option<FinalityResponse>> {
        self.get_block_finality_impl(block).to_rpc_result()
    }

    fn get_transaction_finality(&self, tx_hash: TxHash) -> RpcResult<Option<FinalityResponse>> {
        self.get_transaction_finality_impl(tx_hash).to_rpc_result()
    }

    fn get_batch_finality(&self, batch_number: u64) -> RpcResult<FinalityResponse> {
        self.get_batch_finality_impl(batch_number).to_rpc_result()
    }

    fn get_finality_status(&self) -> RpcResult<NodeFinalityStatus> {
        let finality = self.storage.finality().get_finality_status();
        Ok(NodeFinalityStatus {
            last_sealed_block: self.storage.repository().get_latest_block(),
            last_committed_block: finality.last_committed_block,
            last_committed_batch: finality.last_committed_batch,
            last_executed_block: finality.last_executed_block,
            last_executed_batch: finality.last_executed_batch,
            last_finalized_executed_block: finality.last_finalized_executed_block,
            last_finalized_executed_batch: finality.last_finalized_executed_batch,
        })
    }
}

/// `zks` finality methods result type.
pub type FinalityResult<Ok> = Result<Ok, FinalityError>;

/// General `zks` finality errors.
#[derive(Debug, thiserror::Error)]
pub enum FinalityError {
    #[error(transparent)]
    Batch(#[from] anyhow::Error),
    #[error(transparent)]
    Repository(#[from] RepositoryError),
}
