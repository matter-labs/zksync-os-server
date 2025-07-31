use crate::ReadRpcStorage;
use alloy::primitives::{Address, B256, TxHash, keccak256};
use alloy::rpc::types::Index;
use async_trait::async_trait;
use jsonrpsee::core::RpcResult;
use zksync_os_mini_merkle_tree::MiniMerkleTree;
use zksync_os_rpc_api::zks::ZksApiServer;
use zksync_os_types::rpc::L2ToL1LogProof;

const LOG_PROOF_SUPPORTED_METADATA_VERSION: u8 = 1;

pub struct ZksNamespace<RpcStorage> {
    bridgehub_address: Address,
    storage: RpcStorage,
}

impl<RpcStorage> ZksNamespace<RpcStorage> {
    pub fn new(bridgehub_address: Address, storage: RpcStorage) -> Self {
        Self {
            bridgehub_address,
            storage,
        }
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
        let Some(tx_meta) = self
            .storage
            .repository()
            .get_transaction_meta(tx_hash)
            .unwrap()
        else {
            return Ok(None);
        };
        let Some(batch_number) = self
            .storage
            .batch()
            .get_batch_by_block_number(tx_meta.block_number)
            .unwrap()
        else {
            return Ok(None);
        };
        let Some((from_block, to_block)) = self
            .storage
            .batch()
            .get_batch_range_by_number(batch_number)
            .unwrap()
        else {
            return Ok(None);
        };
        let mut batch_index = None;
        let mut merkle_tree_leaves = vec![];
        for block in from_block..=to_block {
            let Some(block) = self
                .storage
                .repository()
                .get_block_by_number(block)
                .unwrap()
            else {
                return Ok(None);
            };
            for block_tx_hash in block.unseal().body.transactions {
                let receipt = self
                    .storage
                    .repository()
                    .get_transaction_receipt(block_tx_hash)
                    .unwrap()
                    .unwrap();
                if block_tx_hash == tx_hash {
                    batch_index.replace(merkle_tree_leaves.len() + index.0);
                }
                for l2_to_l1_log in receipt.into_l2_to_l1_logs() {
                    merkle_tree_leaves.push(l2_to_l1_log.encode());
                }
            }
        }
        let l1_log_index = batch_index
            .expect("transaction not found in the batch that was supposed to contain it");
        let tree_size = 16384;

        let (local_root, proof) =
            MiniMerkleTree::new(merkle_tree_leaves.into_iter(), Some(tree_size))
                .merkle_root_and_path(l1_log_index);

        // The result should be Keccak(l2_l1_local_root, aggregated_root) but we don't compute aggregated root yet
        let aggregated_root = B256::new([0u8; 32]);
        let root = keccak256([local_root.0, aggregated_root.0].concat());

        let mut log_leaf_proof = proof;
        log_leaf_proof.push(aggregated_root);

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
