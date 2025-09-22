use crate::ReadRpcStorage;
use crate::result::ToRpcResult;
use alloy::primitives::{Address, B256, BlockNumber, TxHash, keccak256};
use alloy::rpc::types::Index;
use async_trait::async_trait;
use jsonrpsee::core::RpcResult;
use std::sync::Arc;
use zksync_os_genesis::{GenesisInput, GenesisInputSource};
use zksync_os_mini_merkle_tree::MiniMerkleTree;
use zksync_os_rpc_api::{types::L2ToL1LogProof, zks::ZksApiServer};
use zksync_os_storage_api::RepositoryError;
use zksync_os_types::L2_TO_L1_TREE_SIZE;

const LOG_PROOF_SUPPORTED_METADATA_VERSION: u8 = 1;

pub struct ZksNamespace<RpcStorage> {
    bridgehub_address: Address,
    storage: RpcStorage,
    genesis_input_source: Arc<dyn GenesisInputSource>,
}

impl<RpcStorage> ZksNamespace<RpcStorage> {
    pub fn new(
        bridgehub_address: Address,
        storage: RpcStorage,
        genesis_input_source: Arc<dyn GenesisInputSource>,
    ) -> Self {
        Self {
            bridgehub_address,
            storage,
            genesis_input_source,
        }
    }
}

impl<RpcStorage: ReadRpcStorage> ZksNamespace<RpcStorage> {
    async fn get_l2_to_l1_log_proof_impl(
        &self,
        tx_hash: TxHash,
        index: Index,
    ) -> ZksResult<Option<L2ToL1LogProof>> {
        let Some(tx_meta) = self.storage.repository().get_transaction_meta(tx_hash)? else {
            return Ok(None);
        };
        let Some(batch_number) = self
            .storage
            .batch()
            .get_batch_by_block_number(tx_meta.block_number)
            .await?
        else {
            return Err(ZksError::NotBatchedYet);
        };
        let Some((from_block, to_block)) = self
            .storage
            .batch()
            .get_batch_range_by_number(batch_number)
            .await?
        else {
            // This should never happen
            tracing::error!(
                block_number = tx_meta.block_number,
                batch_number,
                "block was included in a batch that could not be found"
            );
            return Err(ZksError::NotBatchedYet);
        };
        // Reading from proof storage can return "dirty" data (i.e., not the one that will be
        // finalized on L1). To avoid this, we assert that the batch has been executed as there is
        // no use case for fetching non-executed proofs.
        if self
            .storage
            .finality()
            .get_finality_status()
            .last_executed_block
            < batch_number
        {
            return Err(ZksError::NotExecutedYet);
        }
        let mut batch_index = None;
        let mut merkle_tree_leaves = vec![];
        for block in from_block..=to_block {
            let Some(block) = self.storage.repository().get_block_by_number(block)? else {
                return Err(ZksError::BlockNotAvailable(block));
            };
            for block_tx_hash in block.unseal().body.transactions {
                let Some(receipt) = self
                    .storage
                    .repository()
                    .get_transaction_receipt(block_tx_hash)?
                else {
                    return Err(ZksError::TxNotAvailable(block_tx_hash));
                };
                let l2_to_l1_logs = receipt.into_l2_to_l1_logs();
                if block_tx_hash == tx_hash {
                    if index.0 >= l2_to_l1_logs.len() {
                        return Err(ZksError::IndexOutOfBounds(index.0, l2_to_l1_logs.len()));
                    }
                    batch_index.replace(merkle_tree_leaves.len() + index.0);
                }
                for l2_to_l1_log in l2_to_l1_logs {
                    merkle_tree_leaves.push(l2_to_l1_log.encode());
                }
            }
        }
        let l1_log_index = batch_index
            .expect("transaction not found in the batch that was supposed to contain it");

        let (local_root, proof) =
            MiniMerkleTree::new(merkle_tree_leaves.into_iter(), Some(L2_TO_L1_TREE_SIZE))
                .merkle_root_and_path(l1_log_index);

        // The result should be Keccak(l2_l1_local_root, aggregated_root) but we don't compute aggregated root yet
        let aggregated_root = B256::new([0u8; 32]);
        let root = keccak256([local_root.0, aggregated_root.0].concat());

        let log_leaf_proof = proof
            .into_iter()
            .chain(std::iter::once(aggregated_root))
            .collect::<Vec<_>>();

        // todo: provide batch chain proof when ran on top of gateway
        let (batch_proof_len, batch_chain_proof, is_final_node) = (0, Vec::<B256>::new(), true);

        let proof = {
            let mut metadata = [0u8; 32];
            metadata[0] = LOG_PROOF_SUPPORTED_METADATA_VERSION;
            metadata[1] = log_leaf_proof.len() as u8;
            metadata[2] = batch_proof_len as u8;
            metadata[3] = if is_final_node { 1 } else { 0 };

            let mut result = vec![B256::new(metadata)];

            result.extend(log_leaf_proof);
            result.extend(batch_chain_proof);

            result
        };

        Ok(Some(L2ToL1LogProof {
            batch_number,
            proof,
            root,
            id: l1_log_index as u32,
        }))
    }
}

#[async_trait]
impl<RpcStorage: ReadRpcStorage> ZksApiServer for ZksNamespace<RpcStorage> {
    async fn get_bridgehub_contract(&self) -> RpcResult<Address> {
        Ok(self.bridgehub_address)
    }

    async fn get_l2_to_l1_log_proof(
        &self,
        tx_hash: TxHash,
        index: Index,
    ) -> RpcResult<Option<L2ToL1LogProof>> {
        self.get_l2_to_l1_log_proof_impl(tx_hash, index)
            .await
            .to_rpc_result()
    }

    async fn get_genesis(&self) -> RpcResult<GenesisInput> {
        self.genesis_input_source
            .genesis_input()
            .await
            .map_err(ZksError::GenesisSource)
            .to_rpc_result()
    }
}

/// `zks` namespace result type.
pub type ZksResult<Ok> = Result<Ok, ZksError>;

/// General `zks` namespace errors
#[derive(Debug, thiserror::Error)]
pub enum ZksError {
    #[error("transaction has not been included in an L1 batch yet")]
    NotBatchedYet,
    #[error("L1 batch containing the transaction has not been executed yet")]
    NotExecutedYet,
    /// Historical block could not be found on this node (e.g., pruned).
    #[error("historical block {0} is not available")]
    BlockNotAvailable(BlockNumber),
    /// Historical transaction could not be found on this node (e.g., pruned).
    #[error("historical transaction {0} is not available")]
    TxNotAvailable(TxHash),
    /// Historical transaction could not be found on this node (e.g., pruned).
    #[error(
        "provided L2->L1 log index ({0}) does not exist; there are only {1} L2->L1 logs in the transaction"
    )]
    IndexOutOfBounds(usize, usize),

    #[error(transparent)]
    Batch(#[from] anyhow::Error),
    #[error(transparent)]
    Repository(#[from] RepositoryError),
    #[error(transparent)]
    GenesisSource(anyhow::Error),
}
