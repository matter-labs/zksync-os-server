use crate::imt::{ImtLeaf as EngineImtLeaf, IndexedMerkleTree, calculate_root, indexed_leaf_hash};
use crate::log_proof_utils::{
    L2_MESSAGE_ROOT_ADDRESS, batch_tree_proof, chain_proof_vector, get_chain_log_proof,
};
use crate::result::ToRpcResult;
use crate::{EthCallHandler, ReadRpcStorage};
use alloy::eips::BlockId;
use alloy::primitives::{Address, B256, BlockNumber, TxHash, U64, U256, keccak256};
use alloy::providers::{DynProvider, Provider};
use alloy::rpc::types::{Index, TransactionRequest};
use alloy::sol;
use alloy::sol_types::SolCall;
use anyhow::Context;
use async_trait::async_trait;
use blake2::{Blake2s256, Digest};
use futures::{FutureExt, TryFutureExt};
use jsonrpsee::core::RpcResult;
use ruint::aliases::B160;
use std::sync::Arc;
use zk_ee::common_structs::derive_flat_storage_key;
use zksync_os_contract_interface::IBridgehub;
use zksync_os_genesis::{GenesisInput, GenesisInputSource};
use zksync_os_merkle_tree_api::flat::StorageSlotProof;
use zksync_os_mini_merkle_tree::MiniMerkleTree;
use zksync_os_rpc_api::{
    types::{
        AddressScopedKey, BatchStorageProof, BlockMetadata, ImtInclusionProof, ImtLeaf,
        L1VerificationData, L2ToL1LogProof, LogProofTarget, StateCommitmentPreimage,
    },
    zks::ZksApiServer,
};
use zksync_os_storage_api::{PersistedBatch, RepositoryError, StateError, read_multichain_root};
use zksync_os_types::L2_TO_L1_TREE_SIZE;

const LOG_PROOF_SUPPORTED_METADATA_VERSION: u8 = 1;

/// Canonical L2 address of the atomic-interop commitment tree (`L2InteropCommitmentTree`).
const L2_INTEROP_COMMITMENT_TREE_ADDRESS: Address =
    alloy::primitives::address!("0000000000000000000000000000000000010012");

sol! {
    /// Minimal view surface of `L2InteropCommitmentTree` needed to reconstruct the IMT.
    #[sol(rpc)]
    interface IL2InteropCommitmentTree {
        struct IMTLeaf {
            uint256 value;
            uint256 nextIndex;
            uint256 nextValue;
        }
        function leafCount() external view returns (uint256);
        function leafAt(uint256 index) external view returns (IMTLeaf memory);
    }
}

pub struct ZksNamespace<RpcStorage> {
    bridgehub_address: Address,
    bytecode_supplier_address: Address,
    storage: RpcStorage,
    genesis_input_source: Arc<dyn GenesisInputSource>,
    l2_chain_id: u64,
    gateway_provider: Option<DynProvider>,
    /// L1 provider, used (among other things) to build the L1 MessageRoot
    /// aggregation hop for proofs of L1-settled chains.
    l1_provider: DynProvider,
    /// In-process eth_call handler, used to read commitment-tree leaves at a historical block
    /// when reconstructing IMT inclusion proofs.
    eth_call_handler: EthCallHandler<RpcStorage>,
}

impl<RpcStorage> ZksNamespace<RpcStorage> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        bridgehub_address: Address,
        bytecode_supplier_address: Address,
        storage: RpcStorage,
        genesis_input_source: Arc<dyn GenesisInputSource>,
        l2_chain_id: u64,
        gateway_provider: Option<DynProvider>,
        l1_provider: DynProvider,
        eth_call_handler: EthCallHandler<RpcStorage>,
    ) -> Self {
        Self {
            bridgehub_address,
            bytecode_supplier_address,
            storage,
            genesis_input_source,
            l2_chain_id,
            gateway_provider,
            l1_provider,
            eth_call_handler,
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

        let (batch_proof_len, batch_chain_proof, is_final_node, gateway_block_number) = match &self
            .gateway_provider
        {
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
                            L2_MESSAGE_ROOT_ADDRESS,
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
                            L2_MESSAGE_ROOT_ADDRESS,
                        )
                        .map_err(|e| e.context("batch_tree_proof"));

                        let (chain_proof_vector, (mut batch_chain_proof, batch_proof_len)) =
                            futures::future::try_join(
                                chain_proof_vector_future.boxed(),
                                batch_tree_proof_future.boxed(),
                            )
                            .await?;

                        batch_chain_proof.extend(chain_proof_vector);

                        (
                            batch_proof_len,
                            batch_chain_proof,
                            false,
                            Some(execute_sl_block_number),
                        )
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
                            L2_MESSAGE_ROOT_ADDRESS,
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
                            L2_MESSAGE_ROOT_ADDRESS,
                        )
                        .map_err(|e| e.context("batch_tree_proof"));

                        let (chain_proof_vector, (mut batch_chain_proof, batch_proof_len)) =
                            futures::future::try_join(
                                chain_proof_vector_future.boxed(),
                                batch_tree_proof_future.boxed(),
                            )
                            .await?;

                        batch_chain_proof.extend(chain_proof_vector);

                        (
                            batch_proof_len,
                            batch_chain_proof,
                            false,
                            Some(execute_sl_block_number),
                        )
                    }
                }
            }
            None => match proof_target {
                // L1-settled chain. The interop root for this chain's batch is the GLOBAL L1
                // MessageRoot at the L1 block where the batch was executed (commit `71bc43441`
                // builds the interop tree on L1, keyed by (L1_CHAIN_ID, l1Block)). Build the same
                // aggregation hop the gateway path builds, but against L1's MessageRoot, so the
                // proof is non-final and carries the L1 block as its settlement-layer anchor.
                // Without this the proof terminates at the source batch root, which no consuming
                // chain holds (they import interopRoots[L1_CHAIN_ID][l1Block]), and the
                // atomic-interop deadline has no settlement-layer block to compare against.
                LogProofTarget::MessageRoot => {
                    let execute_sl_block_number = batch
                        .execute_sl_block_number
                        .ok_or(ZksError::BatchNotAvailableYet)?;

                    // The L1 MessageRoot lives at a deployed address (unlike the canonical L2
                    // address used on a gateway); resolve it from the L1 bridgehub.
                    let l1_message_root_address =
                        IBridgehub::new(self.bridgehub_address, &self.l1_provider)
                            .messageRoot()
                            .call()
                            .await
                            .map_err(|e| {
                                anyhow::Error::from(e).context("bridgehub.messageRoot()")
                            })?;

                    let chain_log_proof_future = get_chain_log_proof(
                        self.l2_chain_id,
                        execute_sl_block_number,
                        &self.l1_provider,
                        l1_message_root_address,
                    )
                    .map_err(|e| e.context("get_chain_log_proof (L1)"));

                    let l1_chain_id_future = self
                        .l1_provider
                        .get_chain_id()
                        .map_err(|e| anyhow::Error::from(e).context("get_chain_id (L1)"));

                    let chain_proof_vector_future =
                        futures::future::try_join(chain_log_proof_future, l1_chain_id_future)
                            .map_ok(|(chain_log_proof, l1_chain_id)| {
                                chain_proof_vector(
                                    execute_sl_block_number,
                                    chain_log_proof,
                                    l1_chain_id,
                                )
                            });

                    let batch_tree_proof_future = batch_tree_proof(
                        execute_sl_block_number..=execute_sl_block_number,
                        self.l2_chain_id,
                        batch_number,
                        &self.l1_provider,
                        l1_message_root_address,
                    )
                    .map_err(|e| e.context("batch_tree_proof (L1)"));

                    let (chain_proof_vector, (mut batch_chain_proof, batch_proof_len)) =
                        futures::future::try_join(
                            chain_proof_vector_future.boxed(),
                            batch_tree_proof_future.boxed(),
                        )
                        .await?;

                    batch_chain_proof.extend(chain_proof_vector);

                    (
                        batch_proof_len,
                        batch_chain_proof,
                        false,
                        Some(execute_sl_block_number),
                    )
                }
                // Other targets (e.g. L1 withdrawal-finalization proofs) terminate at L1 as a
                // final node — there is no settlement layer above L1.
                _ => (0, Vec::<B256>::new(), true, None),
            },
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

    /// Read a `L2InteropCommitmentTree` view function at `block`, returning the raw return bytes.
    fn call_commitment_tree(
        &self,
        calldata: alloy::primitives::Bytes,
        block: BlockId,
    ) -> ZksResult<alloy::primitives::Bytes> {
        let request = TransactionRequest::default()
            .to(L2_INTEROP_COMMITMENT_TREE_ADDRESS)
            .input(calldata.into());
        self.eth_call_handler
            .call_impl(request, Some(block), None, None)
            .map_err(|err| ZksError::Batch(anyhow::anyhow!(err)))
    }

    /// Read the index-ordered commitment-tree leaf set as of `block` via `leafCount()` / `leafAt(i)`.
    fn read_commitment_tree(&self, block: BlockId) -> ZksResult<IndexedMerkleTree> {
        let count_bytes = self.call_commitment_tree(
            IL2InteropCommitmentTree::leafCountCall {}
                .abi_encode()
                .into(),
            block,
        )?;
        let leaf_count = IL2InteropCommitmentTree::leafCountCall::abi_decode_returns(&count_bytes)
            .map_err(|err| ZksError::Batch(anyhow::anyhow!(err)))?;
        let leaf_count = leaf_count.to::<u64>();

        let mut leaves = Vec::with_capacity(leaf_count as usize);
        for i in 0..leaf_count {
            let leaf_bytes = self.call_commitment_tree(
                IL2InteropCommitmentTree::leafAtCall {
                    index: U256::from(i),
                }
                .abi_encode()
                .into(),
                block,
            )?;
            let leaf = IL2InteropCommitmentTree::leafAtCall::abi_decode_returns(&leaf_bytes)
                .map_err(|err| ZksError::Batch(anyhow::anyhow!(err)))?;
            leaves.push(EngineImtLeaf {
                value: leaf.value,
                next_index: leaf.nextIndex,
                next_value: leaf.nextValue,
            });
        }
        Ok(IndexedMerkleTree::new(leaves))
    }

    /// Index of the low-nullifier leaf for `value` (the predecessor used when inserting `value`)
    /// against the commitment tree as of `block_number`. `None` if no such leaf exists.
    fn get_imt_low_nullifier_index_impl(
        &self,
        value: U256,
        block_number: u64,
    ) -> ZksResult<Option<u64>> {
        let tree = self.read_commitment_tree(BlockId::from(block_number))?;
        Ok(tree.find_low_nullifier_index(value))
    }

    /// Reconstruct the IMT inclusion proof for the leaf holding `commit_value` against the
    /// commitment tree as of `block_number`.
    ///
    /// Reads the index-ordered leaf set via `leafCount()` / `leafAt(i)` at the historical block,
    /// rebuilds the tree with the off-chain engine (bit-for-bit identical to the on-chain
    /// `IndexedMerkleTree` / `FullMerkle`), and returns the leaf, its index, and its dynamic-height
    /// Merkle path (length == the tree's current height).
    fn get_imt_inclusion_proof_impl(
        &self,
        commit_value: U256,
        block_number: u64,
    ) -> ZksResult<Option<ImtInclusionProof>> {
        let tree = self.read_commitment_tree(BlockId::from(block_number))?;
        let Some(leaf_index) = tree.find_value_index(commit_value) else {
            return Ok(None);
        };
        let leaf = tree.leaves()[leaf_index as usize];
        let root = tree.root();
        let path = tree.merkle_path(leaf_index);

        // Self-verify the produced path against the root (same walk the on-chain `verifyInclusion`
        // performs) so an engine bug surfaces here instead of as an on-chain `executeAtomicBundle`
        // revert. A mismatch is an internal error, not a "leaf absent" (None) result.
        let recomputed = calculate_root(&path, leaf_index, indexed_leaf_hash(&leaf));
        if recomputed != root {
            return Err(ZksError::Batch(anyhow::anyhow!(
                "IMT inclusion proof failed self-verification for commit value {commit_value} at \
                 leaf {leaf_index}: recomputed root {recomputed} != tree root {root}"
            )));
        }

        Ok(Some(ImtInclusionProof {
            chain_imt_root: root,
            leaf: ImtLeaf {
                value: leaf.value,
                next_index: leaf.next_index,
                next_value: leaf.next_value,
            },
            imt_leaf_index: leaf_index,
            imt_proof: path,
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

    async fn get_imt_inclusion_proof(
        &self,
        commit_value: U256,
        block_number: u64,
    ) -> RpcResult<Option<ImtInclusionProof>> {
        self.get_imt_inclusion_proof_impl(commit_value, block_number)
            .to_rpc_result()
    }

    async fn get_imt_low_nullifier_index(
        &self,
        value: U256,
        block_number: u64,
    ) -> RpcResult<Option<u64>> {
        self.get_imt_low_nullifier_index_impl(value, block_number)
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
