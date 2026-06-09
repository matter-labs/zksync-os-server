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
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use zk_ee::common_structs::derive_flat_storage_key;
use zksync_os_genesis::{GenesisInput, GenesisInputSource};
use zksync_os_merkle_tree_api::flat::StorageSlotProof;
use zksync_os_mini_merkle_tree::MiniMerkleTree;
use zksync_os_rpc_api::{
    types::{
        AddressScopedKey, BatchStorageProof, BlockMetadata, L1VerificationData, L2ToL1LogProof,
        LogProofTarget, StateCommitmentPreimage,
    },
    zks::ZksApiServer,
};
use zksync_os_storage_api::{PersistedBatch, RepositoryError, StateError, read_multichain_root};
use zksync_os_types::L2_TO_L1_TREE_SIZE;

const LOG_PROOF_SUPPORTED_METADATA_VERSION: u8 = 1;
const L2_TO_L1_LOG_SERIALIZE_SIZE: usize = 88;

#[derive(Clone)]
struct LocalBatchProofData {
    leaves: Vec<[u8; L2_TO_L1_LOG_SERIALIZE_SIZE]>,
    log_leaf_paths: Vec<Vec<B256>>,
    tx_log_ranges: HashMap<TxHash, (usize, usize)>,
    multichain_root: B256,
    root: B256,
}

#[derive(Clone)]
struct GatewayProofData {
    batch_proof_len: u8,
    batch_chain_proof: Vec<B256>,
    is_final_node: bool,
    gateway_block_number: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct GatewayProofKey {
    proof_target: u8,
    batch_number: u64,
    execute_sl_block_number: u64,
}

#[derive(Default)]
struct LogProofCache {
    local_batches: HashMap<u64, LocalBatchProofData>,
    gateway_proofs: HashMap<GatewayProofKey, GatewayProofData>,
}

pub struct ZksNamespace<RpcStorage> {
    bridgehub_address: Address,
    bytecode_supplier_address: Address,
    storage: RpcStorage,
    genesis_input_source: Arc<dyn GenesisInputSource>,
    l2_chain_id: u64,
    gateway_provider: Option<DynProvider>,
    log_proof_cache: Arc<Mutex<LogProofCache>>,
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
            log_proof_cache: Arc::new(Mutex::new(LogProofCache::default())),
        }
    }
}

impl<RpcStorage: ReadRpcStorage> ZksNamespace<RpcStorage> {
    fn build_local_batch_proof_data(
        &self,
        batch: &PersistedBatch,
    ) -> ZksResult<LocalBatchProofData> {
        let mut tx_log_ranges = HashMap::new();
        let mut merkle_tree_leaves = vec![];

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
                tx_log_ranges.insert(
                    block_tx_hash,
                    (merkle_tree_leaves.len(), l2_to_l1_logs.len()),
                );
                for l2_to_l1_log in l2_to_l1_logs {
                    merkle_tree_leaves.push(l2_to_l1_log.encode());
                }
            }
        }

        let tree =
            MiniMerkleTree::new(merkle_tree_leaves.iter().copied(), Some(L2_TO_L1_TREE_SIZE));
        let local_root = tree.merkle_root();
        let log_leaf_paths = (0..merkle_tree_leaves.len())
            .map(|index| {
                let (path_root, path) = tree.merkle_root_and_path(index);
                debug_assert_eq!(path_root, local_root);
                path
            })
            .collect();

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

        Ok(LocalBatchProofData {
            leaves: merkle_tree_leaves,
            log_leaf_paths,
            tx_log_ranges,
            multichain_root,
            root,
        })
    }

    async fn local_batch_proof_data(
        &self,
        batch: &PersistedBatch,
    ) -> ZksResult<LocalBatchProofData> {
        let batch_number = batch.number();
        if let Some(cached) = self
            .log_proof_cache
            .lock()
            .await
            .local_batches
            .get(&batch_number)
            .cloned()
        {
            return Ok(cached);
        }

        let mut cache = self.log_proof_cache.lock().await;
        if let Some(cached) = cache.local_batches.get(&batch_number).cloned() {
            return Ok(cached);
        }

        let proof_data = self.build_local_batch_proof_data(batch)?;
        if cache.local_batches.len() > 128 {
            cache.local_batches.clear();
        }
        cache.local_batches.insert(batch_number, proof_data.clone());
        Ok(proof_data)
    }

    async fn gateway_proof_data(
        &self,
        batch: &PersistedBatch,
        proof_target: LogProofTarget,
        gateway_provider: &DynProvider,
    ) -> ZksResult<GatewayProofData> {
        let execute_sl_block_number = batch
            .execute_sl_block_number
            .ok_or(ZksError::BatchNotAvailableYet)?;
        let key = GatewayProofKey {
            proof_target: match proof_target {
                LogProofTarget::L1BatchRoot => 0,
                LogProofTarget::MessageRoot => 1,
            },
            batch_number: batch.number(),
            execute_sl_block_number,
        };

        if let Some(cached) = self
            .log_proof_cache
            .lock()
            .await
            .gateway_proofs
            .get(&key)
            .cloned()
        {
            return Ok(cached);
        }

        let batch_number = batch.number();
        let proof_data = match proof_target {
            LogProofTarget::L1BatchRoot => {
                let gateway_batch: PersistedBatch = gateway_provider
                    .raw_request(
                        "unstable_getBatchByBlockNumber".into(),
                        (execute_sl_block_number,),
                    )
                    .await
                    .context("unstable_getBatchByBlockNumber")?;
                let gateway_batch_number = gateway_batch.number();

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
                .map_ok(|(mut chain_log_proof, gw_local_root, gw_chain_id)| {
                    chain_log_proof.chain_id_leaf_proof_mask |=
                        U256::from(1u64 << chain_log_proof.chain_id_leaf_proof.len());
                    chain_log_proof.chain_id_leaf_proof.push(gw_local_root);
                    chain_proof_vector(gateway_batch_number, chain_log_proof, gw_chain_id)
                });

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

                GatewayProofData {
                    batch_proof_len,
                    batch_chain_proof,
                    is_final_node: false,
                    gateway_block_number: Some(execute_sl_block_number),
                }
            }
            LogProofTarget::MessageRoot => {
                let chain_log_proof_future = get_chain_log_proof(
                    self.l2_chain_id,
                    execute_sl_block_number,
                    gateway_provider,
                )
                .map_err(|e| e.context("get_chain_log_proof"));

                let gw_chain_id_future = gateway_provider
                    .get_chain_id()
                    .map_err(|e| anyhow::Error::from(e).context("get_chain_id"));

                let chain_proof_vector_future = futures::future::try_join(
                    chain_log_proof_future,
                    gw_chain_id_future,
                )
                .map_ok(|(chain_log_proof, gw_chain_id)| {
                    chain_proof_vector(execute_sl_block_number, chain_log_proof, gw_chain_id)
                });

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

                GatewayProofData {
                    batch_proof_len,
                    batch_chain_proof,
                    is_final_node: false,
                    gateway_block_number: Some(execute_sl_block_number),
                }
            }
        };

        let mut cache = self.log_proof_cache.lock().await;
        if cache.gateway_proofs.len() > 512 {
            cache.gateway_proofs.clear();
        }
        cache.gateway_proofs.insert(key, proof_data.clone());
        Ok(proof_data)
    }

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

        let batch_number = batch.number();
        let local_proof_data = self.local_batch_proof_data(&batch).await?;
        let Some((first_log_index, log_count)) =
            local_proof_data.tx_log_ranges.get(&tx_hash).copied()
        else {
            return Err(ZksError::TxNotAvailable(tx_hash));
        };
        if index.0 >= log_count {
            return Err(ZksError::IndexOutOfBounds(index.0, log_count));
        }
        let l1_log_index = first_log_index + index.0;

        let proof = local_proof_data
            .log_leaf_paths
            .get(l1_log_index)
            .cloned()
            .ok_or(ZksError::IndexOutOfBounds(
                l1_log_index,
                local_proof_data.leaves.len(),
            ))?;

        let log_leaf_proof = proof
            .into_iter()
            .chain(std::iter::once(local_proof_data.multichain_root))
            .collect::<Vec<_>>();

        let (batch_proof_len, batch_chain_proof, is_final_node, gateway_block_number) =
            match &self.gateway_provider {
                Some(gateway_provider) => {
                    let proof_data = self
                        .gateway_proof_data(&batch, proof_target, gateway_provider)
                        .await?;
                    (
                        proof_data.batch_proof_len,
                        proof_data.batch_chain_proof,
                        proof_data.is_final_node,
                        proof_data.gateway_block_number,
                    )
                }
                None => (0, Vec::<B256>::new(), true, None),
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
            root: local_proof_data.root,
            id: l1_log_index as u32,
            gateway_block_number,
        }))
    }

    fn get_block_metadata_by_number_impl(
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

    fn get_proof_impl(
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

        let l1_verification_data = L1VerificationData {
            batch_number,
            number_of_layer1_txs: batch.batch_info.number_of_layer1_txs,
            priority_operations_hash: batch.batch_info.priority_operations_hash,
            dependency_roots_rolling_hash: batch.batch_info.dependency_roots_rolling_hash,
            l2_to_l1_logs_root_hash: batch.batch_info.l2_to_l1_logs_root_hash,
            commitment: batch.batch_info.commitment,
        };

        Ok(Some(BatchStorageProof {
            address,
            state_commitment_preimage,
            storage_proofs,
            l1_verification_data,
        }))
    }
}

#[async_trait]
impl<RpcStorage: ReadRpcStorage> ZksApiServer for ZksNamespace<RpcStorage> {
    fn get_bridgehub_contract(&self) -> RpcResult<Address> {
        Ok(self.bridgehub_address)
    }

    fn get_bytecode_supplier_contract(&self) -> RpcResult<Address> {
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

    fn get_block_metadata_by_number(&self, block_number: u64) -> RpcResult<Option<BlockMetadata>> {
        self.get_block_metadata_by_number_impl(block_number)
            .to_rpc_result()
    }

    fn get_proof(
        &self,
        account: Address,
        keys: Vec<B256>,
        batch_number: u64,
    ) -> RpcResult<Option<BatchStorageProof>> {
        self.get_proof_impl(account, &keys, batch_number)
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
