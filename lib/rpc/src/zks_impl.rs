use crate::ReadRpcStorage;
use crate::log_proof_utils::{batch_tree_proof, chain_proof_vector, get_chain_log_proof};
use crate::result::ToRpcResult;
use alloy::primitives::{Address, B256, BlockNumber, TxHash, U64, U256, keccak256};
use alloy::providers::{DynProvider, Provider};
use alloy::rpc::types::Index;
use anyhow::Context;
use async_trait::async_trait;
use blake2::{Blake2s256, Digest};
use futures::{FutureExt, TryFutureExt};
use jsonrpsee::core::RpcResult;
use ruint::aliases::B160;
use std::sync::Arc;
use zk_ee::common_structs::derive_flat_storage_key;
use zksync_os_genesis::{GenesisInput, GenesisInputSource};
use zksync_os_merkle_tree_api::flat::StorageSlotProof;
use zksync_os_mini_merkle_tree::MiniMerkleTree;
use zksync_os_rpc_api::{
    types::{
        AddressScopedKey, BatchStorageProof, BlockMetadata, L2ToL1LogProof, LogProofTarget,
        StateCommitmentPreimage,
    },
    zks::ZksApiServer,
};
use zksync_os_storage_api::{PersistedBatch, RepositoryError, StateError, read_multichain_root};
use zksync_os_types::L2_TO_L1_TREE_SIZE;

const LOG_PROOF_SUPPORTED_METADATA_VERSION: u8 = 1;

pub struct ZksNamespace<RpcStorage> {
    bridgehub_address: Address,
    bytecode_supplier_address: Address,
    storage: RpcStorage,
    genesis_input_source: Arc<dyn GenesisInputSource>,
    l2_chain_id: u64,
    gateway_provider: Option<DynProvider>,
}

impl<RpcStorage> ZksNamespace<RpcStorage> {
    pub fn new(
        bridgehub_address: Address,
        bytecode_supplier_address: Address,
        storage: RpcStorage,
        genesis_input_source: Arc<dyn GenesisInputSource>,
        l2_chain_id: u64,
        gateway_provider: Option<DynProvider>,
    ) -> Self {
        Self {
            bridgehub_address,
            bytecode_supplier_address,
            storage,
            genesis_input_source,
            l2_chain_id,
            gateway_provider,
        }
    }
}

impl<RpcStorage: ReadRpcStorage> ZksNamespace<RpcStorage> {
    async fn get_l2_to_l1_log_proof_impl(
        &self,
        tx_hash: TxHash,
        index: Index,
        proof_target: LogProofTarget,
    ) -> ZksResult<Option<L2ToL1LogProof>> {
        let Some(tx_meta) = self.storage.repository().get_transaction_meta(tx_hash)? else {
            return Ok(None);
        };
        let block_number = tx_meta.block_number;
        let Some(batch) = self
            .storage
            .batch()
            .get_batch_by_block_number(block_number)?
        else {
            return Ok(None);
        };

        let mut batch_index = None;
        let mut merkle_tree_leaves = vec![];
        let batch_number = batch.number();
        for block in batch.block_range.clone() {
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

        let state = self.storage.state_view_at(*batch.block_range.end())?;
        let last_block_replay_record = self
            .storage
            .replay_storage()
            .get_replay_record(*batch.block_range.end())
            .ok_or(ZksError::BlockNotAvailable(*batch.block_range.end()))?;
        let multichain_root = if last_block_replay_record.protocol_version.is_post_v31() {
            read_multichain_root(state)
        } else {
            B256::new([0u8; 32])
        };
        let root = keccak256([local_root.0, multichain_root.0].concat());

        let log_leaf_proof = proof
            .into_iter()
            .chain(std::iter::once(multichain_root))
            .collect::<Vec<_>>();

        let (batch_proof_len, batch_chain_proof, is_final_node) = match &self.gateway_provider {
            Some(gateway_provider) => {
                let execute_sl_block_number = batch
                    .execute_sl_block_number
                    .ok_or(ZksError::BatchNotAvailableYet)?;

                match proof_target {
                    LogProofTarget::L1BatchRoot => {
                        let gateway_batch: PersistedBatch = gateway_provider
                            .raw_request(
                                "unstable_getBatchByBlockNumber".into(),
                                (execute_sl_block_number,),
                            )
                            .await
                            .context("unstable_getBatchByBlockNumber")?;
                        let gateway_batch_number = gateway_batch.number();

                        // "batch" and "chain" parts can be fetched in parallel, so we prepare futures and join them at the end.
                        let chain_log_proof_future = get_chain_log_proof(
                            self.l2_chain_id,
                            gateway_batch.last_block_number(),
                            gateway_provider,
                        )
                        .map_err(|e| e.context("get_chain_log_proof"));

                        let gw_local_root_future = gateway_provider
                            .raw_request("unstable_getLocalRoot".into(), (gateway_batch_number,))
                            .map_err(|e| anyhow::Error::from(e).context("unstable_getLocalRoot"));

                        let gw_chain_id_future = gateway_provider
                            .get_chain_id()
                            .map_err(|e| anyhow::Error::from(e).context("get_chain_id"));

                        let chain_proof_vector_future = futures::future::try_join3(
                            chain_log_proof_future,
                            gw_local_root_future,
                            gw_chain_id_future,
                        )
                        .map_ok(
                            |(mut chain_log_proof, gw_local_root, gw_chain_id)| {
                                // Chain tree is the right subtree of the aggregated tree.
                                // We append root of the left subtree to form full proof.
                                chain_log_proof.chain_id_leaf_proof_mask |=
                                    U256::from(1u64 << chain_log_proof.chain_id_leaf_proof.len());
                                chain_log_proof.chain_id_leaf_proof.push(gw_local_root);
                                chain_proof_vector(
                                    gateway_batch_number,
                                    chain_log_proof,
                                    gw_chain_id,
                                )
                            },
                        );

                        let batch_tree_proof_future = batch_tree_proof(
                            gateway_batch.block_range.clone(),
                            self.l2_chain_id,
                            batch_number,
                            gateway_provider,
                        )
                        .map_err(|e| e.context("batch_tree_proof"));

                        let (chain_proof_vector, (mut batch_chain_proof, batch_proof_len)) =
                            futures::future::try_join(
                                chain_proof_vector_future.boxed(),
                                batch_tree_proof_future.boxed(),
                            )
                            .await?;

                        batch_chain_proof.extend(chain_proof_vector);

                        (batch_proof_len, batch_chain_proof, false)
                    }
                    LogProofTarget::MessageRoot => {
                        // For the "until msg root" format the chain proof is taken at the specific
                        // SL block where this chain batch was executed (not at the end of the SL
                        // L1 batch). The proof goes from the batch leaf directly to the block-level
                        // message root, so no local-root extension is required.
                        let chain_log_proof_future = get_chain_log_proof(
                            self.l2_chain_id,
                            execute_sl_block_number,
                            gateway_provider,
                        )
                        .map_err(|e| e.context("get_chain_log_proof"));

                        let gw_chain_id_future = gateway_provider
                            .get_chain_id()
                            .map_err(|e| anyhow::Error::from(e).context("get_chain_id"));

                        let chain_proof_vector_future =
                            futures::future::try_join(chain_log_proof_future, gw_chain_id_future)
                                .map_ok(|(chain_log_proof, gw_chain_id)| {
                                    chain_proof_vector(
                                        execute_sl_block_number,
                                        chain_log_proof,
                                        gw_chain_id,
                                    )
                                });

                        // The batch tree proof uses only the single execution block so that the
                        // resulting root matches the block-level message root.
                        let batch_tree_proof_future = batch_tree_proof(
                            execute_sl_block_number..=execute_sl_block_number,
                            self.l2_chain_id,
                            batch_number,
                            gateway_provider,
                        )
                        .map_err(|e| e.context("batch_tree_proof"));

                        let (chain_proof_vector, (mut batch_chain_proof, batch_proof_len)) =
                            futures::future::try_join(
                                chain_proof_vector_future.boxed(),
                                batch_tree_proof_future.boxed(),
                            )
                            .await?;

                        batch_chain_proof.extend(chain_proof_vector);

                        (batch_proof_len, batch_chain_proof, false)
                    }
                }
            }
            None => (0, Vec::<B256>::new(), true),
        };

        let proof = {
            let mut metadata = [0u8; 32];
            metadata[0] = LOG_PROOF_SUPPORTED_METADATA_VERSION;
            metadata[1] = log_leaf_proof.len() as u8;
            metadata[2] = batch_proof_len;
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

    async fn get_block_metadata_by_number_impl(
        &self,
        block_number: u64,
    ) -> ZksResult<Option<BlockMetadata>> {
        let Some(block) = self
            .storage
            .replay_storage()
            .get_replay_record(block_number)
        else {
            return Ok(None);
        };

        let pubdata_price_per_byte = block.block_context.pubdata_price;
        let native_price = block.block_context.native_price;
        let execution_version = block.block_context.execution_version;
        Ok(Some(BlockMetadata {
            pubdata_price_per_byte,
            native_price,
            execution_version,
        }))
    }

    async fn get_proof_impl(
        &self,
        address: Address,
        keys: &[B256],
        batch_number: u64,
    ) -> ZksResult<Option<BatchStorageProof>> {
        let Some(batch) = self.storage.batch().get_batch_by_number(batch_number)? else {
            return Ok(None);
        };
        let last_block_number = batch.last_block_number();

        let last_block_replay = self
            .storage
            .replay_storage()
            .get_replay_record(last_block_number)
            .with_context(|| {
                format!("missing last block {last_block_number} for batch #{batch_number}")
            })?;
        let block_hashes = last_block_replay.block_context.block_hashes;

        let last_block = self
            .storage
            .repository()
            .get_block_by_number(last_block_number)?
            .with_context(|| {
                format!("missing last block {last_block_number} for batch #{batch_number}")
            })?
            .into_inner();
        let last_block_header_for_hashing = alloy::consensus::Header {
            // `logs_bloom` must be zeroed out when computing block hashes due to how
            // block hashes are defined elsewhere in the codebase.
            logs_bloom: alloy::primitives::Bloom::default(),
            ..last_block.header
        };
        let last_block_hash = last_block_header_for_hashing.hash_slow();

        let last_256_block_hashes_blake = {
            let mut blocks_hasher = Blake2s256::new();
            for block_hash in &block_hashes.0[1..] {
                blocks_hasher.update(block_hash.to_be_bytes::<32>());
            }
            blocks_hasher.update(last_block_hash.as_slice());
            B256::from_slice(&blocks_hasher.finalize())
        };

        let address_for_keys = B160::from_be_bytes(address.into_array());
        let flat_keys: Vec<_> = keys
            .iter()
            .map(|account_key| {
                let flat_key = derive_flat_storage_key(&address_for_keys, &account_key.0.into());
                B256::new(flat_key.as_u8_array())
            })
            .collect();
        // We query tree version by the *block* number because the tree is updated on each block,
        // rather than once per batch.
        let Some((flat_proofs, tree_output)) = self
            .storage
            .tree()
            .prove_flat(last_block_number, &flat_keys)?
        else {
            return Ok(None);
        };

        // Swap flat keys in the proofs back to address-scoped keys
        let storage_proofs: Vec<_> = flat_proofs
            .into_iter()
            .zip(keys)
            .map(|(proof, &key)| StorageSlotProof {
                key: AddressScopedKey(key),
                proof: proof.proof,
            })
            .collect();

        let state_commitment_preimage = StateCommitmentPreimage {
            next_free_slot: U64::from(tree_output.leaf_count),
            block_number: U64::from(last_block_number),
            last_256_block_hashes_blake,
            last_block_timestamp: U64::from(last_block.header.timestamp),
        };

        let recovered = state_commitment_preimage.hash(tree_output.root_hash);
        if batch.batch_info.state_commitment != recovered {
            let err = anyhow::anyhow!(
                "Mismatch between stored ({stored:?}) and recovered ({recovered:?}) state commitments \
                 for batch #{batch_number}; preimage = {state_commitment_preimage:?}, tree_output = {tree_output:?}",
                stored = batch.batch_info.state_commitment
            );
            return Err(err.into());
        }

        Ok(Some(BatchStorageProof {
            address,
            state_commitment_preimage,
            storage_proofs,
        }))
    }
}

#[async_trait]
impl<RpcStorage: ReadRpcStorage> ZksApiServer for ZksNamespace<RpcStorage> {
    async fn get_bridgehub_contract(&self) -> RpcResult<Address> {
        Ok(self.bridgehub_address)
    }

    async fn get_bytecode_supplier_contract(&self) -> RpcResult<Address> {
        Ok(self.bytecode_supplier_address)
    }

    async fn get_l2_to_l1_log_proof(
        &self,
        tx_hash: TxHash,
        index: Index,
        proof_target: Option<LogProofTarget>,
    ) -> RpcResult<Option<L2ToL1LogProof>> {
        self.get_l2_to_l1_log_proof_impl(tx_hash, index, proof_target.unwrap_or_default())
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

    async fn get_block_metadata_by_number(
        &self,
        block_number: u64,
    ) -> RpcResult<Option<BlockMetadata>> {
        self.get_block_metadata_by_number_impl(block_number)
            .await
            .to_rpc_result()
    }

    async fn get_proof(
        &self,
        account: Address,
        keys: Vec<B256>,
        batch_number: u64,
    ) -> RpcResult<Option<BatchStorageProof>> {
        self.get_proof_impl(account, &keys, batch_number)
            .await
            .to_rpc_result()
    }
}

/// `zks` namespace result type.
pub type ZksResult<Ok> = Result<Ok, ZksError>;

/// General `zks` namespace errors
#[derive(Debug, thiserror::Error)]
pub enum ZksError {
    /// Block is executed according to L1 but hasn't been indexed by this node yet. Client needs to
    /// retry after some time passes. For early blocks in old testnets it can also mean that the
    /// batch is legacy and the node does not index it anymore.
    #[error(
        "L1 batch containing the transaction has not been finalized or indexed by this node yet"
    )]
    BatchNotAvailableYet,
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
    #[error(transparent)]
    State(#[from] StateError),
}
