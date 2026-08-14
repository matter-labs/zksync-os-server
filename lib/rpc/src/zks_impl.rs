use crate::imt::{IndexedMerkleTree, calculate_root, indexed_leaf_hash};
use crate::interop_commitment_tree::{InteropCommitmentTreeError, InteropCommitmentTreeReader};
use crate::log_proof_utils::{
    MessageRootProofExtension, assemble_log_proof, build_message_root_proof_extension,
};
use crate::result::ToRpcResult;
use crate::{EthCallHandler, ReadRpcStorage};
use alloy::primitives::{Address, B256, BlockNumber, TxHash, U64, U256, keccak256};
use alloy::providers::DynProvider;
use alloy::rpc::types::Index;
use anyhow::Context;
use async_trait::async_trait;
use blake2::{Blake2s256, Digest};
use jsonrpsee::core::RpcResult;
use ruint::aliases::B160;
use std::sync::Arc;
use zk_ee::common_structs::derive_flat_storage_key;
use zksync_os_batch_types::chain_batch_root::{
    IMT_BEGIN_ROOT_LEAF_INDEX, IMT_END_ROOT_LEAF_INDEX, LOGS_ROOT_LEAF_INDEX,
    chain_batch_root_leaf_siblings, compute_chain_batch_root,
};
use zksync_os_contract_interface::IBridgehub;
use zksync_os_genesis::{GenesisInput, GenesisInputSource};
use zksync_os_merkle_tree_api::flat::StorageSlotProof;
use zksync_os_mini_merkle_tree::MiniMerkleTree;
use zksync_os_rpc_api::{
    types::{
        AddressScopedKey, BatchStorageProof, BlockMetadata, ImtLeaf, ImtProof, L1VerificationData,
        L2ToL1LogProof, LogProofTarget, StateCommitmentPreimage,
    },
    zks::ZksApiServer,
};
use zksync_os_storage_api::{
    PersistedBatch, RepositoryError, StateError, read_commitment_tree_root, read_multichain_root,
};
use zksync_os_types::{L2_TO_L1_LOG_SERIALIZE_SIZE, L2_TO_L1_TREE_SIZE, ProtocolSemanticVersion};

pub struct ZksNamespace<RpcStorage> {
    bridgehub_address: Address,
    bytecode_supplier_address: Address,
    storage: RpcStorage,
    genesis_input_source: Arc<dyn GenesisInputSource>,
    l2_chain_id: u64,
    /// Queries the deployed L1 MessageRoot when an interop proof needs its aggregation segments.
    l1_provider: DynProvider,
    commitment_tree_reader: InteropCommitmentTreeReader<RpcStorage>,
}

impl<RpcStorage> ZksNamespace<RpcStorage> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        bridgehub_address: Address,
        bytecode_supplier_address: Address,
        storage: RpcStorage,
        genesis_input_source: Arc<dyn GenesisInputSource>,
        l2_chain_id: u64,
        l1_provider: DynProvider,
        eth_call_handler: EthCallHandler<RpcStorage>,
    ) -> Self {
        Self {
            bridgehub_address,
            bytecode_supplier_address,
            storage,
            genesis_input_source,
            l2_chain_id,
            l1_provider,
            commitment_tree_reader: InteropCommitmentTreeReader::new(eth_call_handler),
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

        let batch_number = batch.number();
        let (merkle_tree_leaves, batch_index) =
            self.batch_l2_to_l1_log_leaves(&batch, Some((tx_hash, index.0)))?;
        let l1_log_index = batch_index
            .expect("transaction not found in the batch that was supposed to contain it");

        let (local_root, proof) =
            MiniMerkleTree::new(merkle_tree_leaves.into_iter(), Some(L2_TO_L1_TREE_SIZE))
                .merkle_root_and_path(l1_log_index);

        let state = self.storage.state_view_at(*batch.block_range.end())?;
        let protocol_version = &self.protocol_version_at(*batch.block_range.end())?;
        let (root, log_leaf_proof) = if protocol_version.supports_l1_interop() {
            // From v32 the chain batch root is the 8-leaf tree (see `chain_batch_root`); the
            // log-leaf path extends with leaf 0's three siblings.
            let multichain_root = read_multichain_root(state);
            let (imt_root_begin, imt_root_end) = self.imt_boundary_roots(&batch)?;
            let root =
                compute_chain_batch_root(local_root, multichain_root, imt_root_begin, imt_root_end);
            let siblings = chain_batch_root_leaf_siblings(
                LOGS_ROOT_LEAF_INDEX,
                local_root,
                multichain_root,
                imt_root_begin,
                imt_root_end,
            );
            (root, proof.into_iter().chain(siblings).collect::<Vec<_>>())
        } else {
            let multichain_root = if protocol_version.is_post_v31() {
                read_multichain_root(state)
            } else {
                B256::new([0u8; 32])
            };
            (
                keccak256([local_root.0, multichain_root.0].concat()),
                proof
                    .into_iter()
                    .chain(std::iter::once(multichain_root))
                    .collect::<Vec<_>>(),
            )
        };

        let (proof_extension, settlement_layer_block_number) = match proof_target {
            // Other chains do not store this source batch root. They import the shared root
            // keyed by `(L1 chain id, L1 block)`, so continue through the source chain's batch
            // tree and L1's chain tree. The execution block identifies the exact shared root
            // and supplies the settlement-layer timestamp boundary used by atomic interop.
            LogProofTarget::MessageRoot => {
                if !protocol_version.supports_l1_interop() {
                    return Err(ZksError::MessageRootProofUnsupportedProtocolVersion {
                        batch_number,
                        required_protocol_version:
                            ProtocolSemanticVersion::MIN_VERSION_WITH_L1_INTEROP,
                        actual_protocol_version: protocol_version.clone(),
                    });
                }

                let execute_sl_block_number = Self::batch_execute_sl_block(&batch)?;
                let proof_extension = self
                    .message_root_proof_extension(batch_number, execute_sl_block_number)
                    .await?;

                (Some(proof_extension), Some(execute_sl_block_number))
            }
            // Other targets (e.g. L1 withdrawal-finalization proofs) terminate at L1 as a
            // final node — there is no settlement layer above L1.
            _ => (None, None),
        };

        let proof = assemble_log_proof(log_leaf_proof, proof_extension);

        Ok(Some(L2ToL1LogProof {
            batch_number,
            proof,
            root,
            id: l1_log_index as u32,
            settlement_layer_block_number,
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

    /// Index of the low-nullifier leaf for `value` (the predecessor used when inserting `value`)
    /// against the commitment tree as of `block_number`. `None` if no such leaf exists.
    fn get_imt_low_nullifier_index_impl(
        &self,
        value: U256,
        block_number: u64,
    ) -> ZksResult<Option<u64>> {
        self.ensure_imt_supported(block_number)?;
        let tree = self.commitment_tree_reader.read(block_number.into())?;
        Ok(tree.find_low_nullifier_index(value))
    }

    /// Protocol version the block was produced under.
    fn protocol_version_at(&self, block_number: u64) -> ZksResult<ProtocolSemanticVersion> {
        Ok(self
            .storage
            .replay_storage()
            .get_replay_record(block_number)
            .ok_or(ZksError::BlockNotAvailable(block_number))?
            .protocol_version)
    }

    /// IMT proofs exist only for state produced under v32+: earlier blocks have no commitment
    /// tree contract to read and pre-v32 batches carry no IMT leaves in their chain batch root.
    fn ensure_imt_supported(&self, block_number: u64) -> ZksResult<()> {
        let version = self.protocol_version_at(block_number)?;
        if !version.supports_l1_interop() {
            return Err(ZksError::ImtProofUnsupportedProtocolVersion {
                block_number,
                required_protocol_version: ProtocolSemanticVersion::MIN_VERSION_WITH_L1_INTEROP,
                actual_protocol_version: version,
            });
        }
        Ok(())
    }

    /// The batch's L1 execution block — the settlement anchor every MessageRoot-extended proof
    /// hangs off. Retryable precondition: "not available" is the phrase pollers match on.
    fn batch_execute_sl_block(batch: &PersistedBatch) -> ZksResult<u64> {
        batch.execute_sl_block_number.ok_or_else(|| {
            ZksError::Batch(anyhow::anyhow!(
                "batch {} has not been executed on L1 yet; settlement proof not available yet",
                batch.number()
            ))
        })
    }

    /// Aggregation hops from the chain batch root to the shared root imported for the batch's
    /// L1 execution block. MessageRoot is a deployed L1 contract rather than a fixed-address
    /// system contract, so its address comes from Bridgehub.
    async fn message_root_proof_extension(
        &self,
        batch_number: u64,
        execute_sl_block_number: u64,
    ) -> ZksResult<MessageRootProofExtension> {
        let l1_message_root_address = IBridgehub::new(self.bridgehub_address, &self.l1_provider)
            .messageRoot()
            .call()
            .await
            .context("bridgehub.messageRoot()")?;
        Ok(build_message_root_proof_extension(
            self.l2_chain_id,
            batch_number,
            execute_sl_block_number,
            &self.l1_provider,
            l1_message_root_address,
        )
        .await
        .context("build MessageRoot proof extension")?)
    }

    /// Collects the batch's L2->L1 log leaves in tree order; when `locate` is given, also
    /// returns the tree position of log `index` within transaction `tx_hash`.
    fn batch_l2_to_l1_log_leaves(
        &self,
        batch: &PersistedBatch,
        locate: Option<(TxHash, usize)>,
    ) -> ZksResult<(Vec<[u8; L2_TO_L1_LOG_SERIALIZE_SIZE]>, Option<usize>)> {
        let mut leaves = vec![];
        let mut located = None;
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
                if let Some((tx_hash, index)) = locate
                    && block_tx_hash == tx_hash
                {
                    if index >= l2_to_l1_logs.len() {
                        return Err(ZksError::IndexOutOfBounds(index, l2_to_l1_logs.len()));
                    }
                    located.replace(leaves.len() + index);
                }
                for l2_to_l1_log in l2_to_l1_logs {
                    leaves.push(l2_to_l1_log.encode());
                }
            }
        }
        Ok((leaves, located))
    }

    /// The batch's IMT boundary snapshots as the bootloader commits them: `begin` = before the
    /// batch's first block, `end` = after its last.
    fn imt_boundary_roots(&self, batch: &PersistedBatch) -> ZksResult<(B256, B256)> {
        let first_block = *batch.block_range.start();
        let begin =
            read_commitment_tree_root(self.storage.state_view_at(first_block.saturating_sub(1))?);
        let end = read_commitment_tree_root(self.storage.state_view_at(*batch.block_range.end())?);
        Ok((begin, end))
    }

    /// The batch's local L2->L1 logs tree root (chain-batch-root leaf 0), recomputed from the
    /// batch's blocks.
    fn batch_local_logs_root(&self, batch: &PersistedBatch) -> ZksResult<B256> {
        let (leaves, _) = self.batch_l2_to_l1_log_leaves(batch, None)?;
        Ok(MiniMerkleTree::new(leaves.into_iter(), Some(L2_TO_L1_TREE_SIZE)).merkle_root())
    }

    /// Settlement half of an atomic-interop IMT proof: metadata word, the chain-batch-root
    /// siblings for `imt_root_leaf_index`, then the aggregation hops — the `bytes32[]` that
    /// `AtomicInteropProof._authenticateRoot` consumes.
    async fn build_imt_settlement_proof(
        &self,
        batch: &PersistedBatch,
        imt_root_leaf_index: u64,
        execute_sl_block_number: u64,
    ) -> ZksResult<Vec<B256>> {
        let local_logs_root = self.batch_local_logs_root(batch)?;
        let multichain_root =
            read_multichain_root(self.storage.state_view_at(*batch.block_range.end())?);
        let (imt_root_begin, imt_root_end) = self.imt_boundary_roots(batch)?;
        let siblings = chain_batch_root_leaf_siblings(
            imt_root_leaf_index,
            local_logs_root,
            multichain_root,
            imt_root_begin,
            imt_root_end,
        );
        let extension = self
            .message_root_proof_extension(batch.number(), execute_sl_block_number)
            .await?;
        Ok(assemble_log_proof(siblings.to_vec(), Some(extension)))
    }

    /// Shared tail of both IMT proof endpoints: authenticates `leaf_index` against `tree`,
    /// cross-checks the engine root against the stored root at `boundary_block`, and attaches
    /// the settlement half for `imt_root_leaf_index`.
    #[allow(clippy::too_many_arguments)]
    async fn assemble_imt_proof(
        &self,
        batch: &PersistedBatch,
        tree: &IndexedMerkleTree,
        leaf_index: u64,
        boundary_block: u64,
        imt_root_leaf_index: u64,
        execute_sl_block_number: u64,
        commit_value: U256,
    ) -> ZksResult<ImtProof> {
        let leaf = tree.leaves()[leaf_index as usize];
        let root = tree.root();
        let path = tree.merkle_path(leaf_index);

        // Self-verify the produced path against the root (same walk the on-chain verifiers
        // perform) so an engine bug surfaces here instead of as an on-chain revert. A mismatch
        // is an internal error, not a "leaf absent" (None) result.
        let recomputed = calculate_root(&path, leaf_index, indexed_leaf_hash(&leaf));
        if recomputed != root {
            return Err(InteropCommitmentTreeError::ProofMismatch {
                commit_value,
                leaf_index,
                recomputed_root: recomputed,
                tree_root: root,
            }
            .into());
        }
        // Cross-check against the tree contract's storage at the proven boundary — the value
        // the settlement proof authenticates.
        let stored_root = read_commitment_tree_root(self.storage.state_view_at(boundary_block)?);
        if root != stored_root {
            return Err(ZksError::Batch(anyhow::anyhow!(
                "IMT engine root {root} != stored boundary root {stored_root} at block \
                 {boundary_block} for batch {}",
                batch.number()
            )));
        }

        let settlement_proof = self
            .build_imt_settlement_proof(batch, imt_root_leaf_index, execute_sl_block_number)
            .await?;

        Ok(ImtProof {
            batch_number: batch.number(),
            settlement_block_number: execute_sl_block_number,
            proves_against_begin_root: imt_root_leaf_index == IMT_BEGIN_ROOT_LEAF_INDEX,
            chain_imt_root: root,
            settlement_proof,
            leaf: ImtLeaf {
                value: leaf.value,
                next_index: leaf.next_index,
                next_value: leaf.next_value,
            },
            imt_leaf_index: leaf_index,
            imt_proof: path,
        })
    }

    /// Reconstructs the complete atomic-interop inclusion proof for the leaf holding
    /// `commit_value`.
    ///
    /// The IMT half is anchored at the **batch-end** root (chain-batch-root leaf 3) of the batch
    /// containing `block_number`: the tree is read at the batch's last block and the response
    /// includes the stored leaf preimage, its insertion-order index, and one sibling per level in
    /// the dynamic-height tree. The settlement half authenticates that root against the interop
    /// root imported for the batch's L1 execution. `None` means the requested value is absent
    /// from the batch-end tree.
    async fn get_imt_inclusion_proof_impl(
        &self,
        commit_value: U256,
        block_number: u64,
    ) -> ZksResult<Option<ImtProof>> {
        let Some(batch) = self
            .storage
            .batch()
            .get_batch_by_block_number(block_number)?
        else {
            return Err(ZksError::Batch(anyhow::anyhow!(
                "batch for block {block_number} is not available yet"
            )));
        };
        let batch_end_block = *batch.block_range.end();
        self.ensure_imt_supported(batch_end_block)?;
        // Retryable precondition first — the tree rebuild below costs one eth_call per leaf.
        let execute_sl_block_number = Self::batch_execute_sl_block(&batch)?;

        let tree = self.commitment_tree_reader.read(batch_end_block.into())?;
        let Some(leaf_index) = tree.find_value_index(commit_value) else {
            return Ok(None);
        };
        self.assemble_imt_proof(
            &batch,
            &tree,
            leaf_index,
            batch_end_block,
            IMT_END_ROOT_LEAF_INDEX,
            execute_sl_block_number,
            commit_value,
        )
        .await
        .map(Some)
    }

    /// Reconstructs the complete atomic-interop timeout (non-inclusion) proof for `commit_value`
    /// against the **batch-begin** IMT root (chain-batch-root leaf 2) of `batch_number`.
    ///
    /// The IMT half proves the low-nullifier (predecessor) leaf bracketing the absent value as of
    /// the block before the batch's first block — the exact snapshot the bootloader committed as
    /// the begin leaf. Serves the begin branch of `verifyTimeoutAbsence`; `None` means the value
    /// IS present (no bracket exists).
    async fn get_imt_non_inclusion_proof_impl(
        &self,
        commit_value: U256,
        batch_number: u64,
    ) -> ZksResult<Option<ImtProof>> {
        let Some(batch) = self.storage.batch().get_batch_by_number(batch_number)? else {
            return Err(ZksError::Batch(anyhow::anyhow!(
                "batch {batch_number} is not available yet"
            )));
        };
        self.ensure_imt_supported(*batch.block_range.end())?;
        let begin_block = batch.block_range.start().saturating_sub(1);
        // The tree contract exists from genesis on fresh v32 chains but only from the upgrade
        // block on upgraded ones — a begin boundary in pre-v32 history has no tree to read.
        if begin_block > 0 && !self.protocol_version_at(begin_block)?.supports_l1_interop() {
            return Err(ZksError::ImtBeginBoundaryPredatesTree { batch_number });
        }
        // Retryable precondition first — the tree rebuild below costs one eth_call per leaf.
        let execute_sl_block_number = Self::batch_execute_sl_block(&batch)?;

        let tree = self.commitment_tree_reader.read(begin_block.into())?;
        let Some(leaf_index) = tree.find_low_nullifier_index(commit_value) else {
            return Ok(None);
        };
        self.assemble_imt_proof(
            &batch,
            &tree,
            leaf_index,
            begin_block,
            IMT_BEGIN_ROOT_LEAF_INDEX,
            execute_sl_block_number,
            commit_value,
        )
        .await
        .map(Some)
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
    ) -> RpcResult<Option<ImtProof>> {
        self.get_imt_inclusion_proof_impl(commit_value, block_number)
            .await
            .to_rpc_result()
    }

    async fn get_imt_non_inclusion_proof(
        &self,
        commit_value: U256,
        batch_number: u64,
    ) -> RpcResult<Option<ImtProof>> {
        self.get_imt_non_inclusion_proof_impl(commit_value, batch_number)
            .await
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
    /// The requested batch predates the L1 MessageRoot proof format.
    #[error(
        "MessageRoot proofs require protocol version {required_protocol_version} or newer; batch \
         {batch_number} uses {actual_protocol_version}"
    )]
    MessageRootProofUnsupportedProtocolVersion {
        batch_number: u64,
        required_protocol_version: ProtocolSemanticVersion,
        actual_protocol_version: ProtocolSemanticVersion,
    },
    /// The requested state predates the atomic-interop commitment tree.
    #[error(
        "IMT proofs require protocol version {required_protocol_version} or newer; block \
         {block_number} was produced under {actual_protocol_version}"
    )]
    ImtProofUnsupportedProtocolVersion {
        block_number: u64,
        required_protocol_version: ProtocolSemanticVersion,
        actual_protocol_version: ProtocolSemanticVersion,
    },
    /// Permanent for this batch — deliberately does NOT contain the retryable
    /// "not available" phrase.
    #[error(
        "batch {batch_number} is the chain's first v32 batch; its begin boundary predates the \
         interop commitment tree, so no begin-root IMT proof exists for it"
    )]
    ImtBeginBoundaryPredatesTree { batch_number: u64 },

    #[error(transparent)]
    CommitmentTree(#[from] InteropCommitmentTreeError),

    #[error(transparent)]
    Batch(#[from] anyhow::Error),
    #[error(transparent)]
    Repository(#[from] RepositoryError),
    #[error(transparent)]
    GenesisSource(anyhow::Error),
    #[error(transparent)]
    State(#[from] StateError),
}
