use crate::NativeBatchRunOutput;
use alloy::primitives::{B256, ruint::aliases::B160};
use anyhow::Context as _;
use std::collections::{HashMap, VecDeque};
use zk_ee_0_4_0::common_structs::{ProofData, da_commitment_scheme::DACommitmentScheme};
use zk_ee_0_4_0::system::metadata::zk_metadata::{BlockHashes, BlockMetadataFromOracle};
use zk_ee_0_4_0::utils::Bytes32;
use zk_os_basic_system_0_4_0::system_implementation::flat_storage_model::FlatStorageLeaf;
use zk_os_forward_system_0_4_0::run::{
    BatchBlockInput, BatchState as ForwardBatchState, LeafProof, ReadStorageTree,
    StorageCommitment, generate_batch_proof_input,
};
use zksync_os_interface::traits::{
    PreimageSource as InterfacePreimageSource, ReadStorage as InterfaceReadStorage, TxListSource,
};
use zksync_os_merkle_tree::{
    Blake2Hasher, MerkleTree, MerkleTreeProver, RocksDBWrapper, api::flat,
};
use zksync_os_storage_api::{ReadStateHistory, ReplayRecord, ViewState};
use zksync_os_types::{PubdataMode, ZksyncOsEncode};

const TREE_DEPTH: u8 = 64;

pub(crate) fn generate_batch_run<ReadState: ReadStateHistory>(
    replay_records: &[ReplayRecord],
    read_state: &ReadState,
    merkle_tree: MerkleTree<RocksDBWrapper>,
    pubdata_mode: PubdataMode,
) -> anyhow::Result<NativeBatchRunOutput> {
    anyhow::ensure!(
        !replay_records.is_empty(),
        "batch prover input requires at least one block",
    );

    let first_replay_record = &replay_records[0];
    let first_state_version = first_replay_record
        .block_context
        .block_number
        .checked_sub(1)
        .context("batch prover input requires a parent state version")?;
    let (root_hash, leaf_count) = merkle_tree
        .root_info(first_state_version)?
        .context("missing Merkle tree state for the first V8 batch block")?;

    let initial_proof_data = ProofData {
        state_root_view: StorageCommitment {
            root: bytes32_from_b256(root_hash),
            next_free_slot: leaf_count,
        },
        last_block_timestamp: first_replay_record.previous_block_timestamp,
    };

    let state_views = replay_records
        .iter()
        .map(|replay_record| {
            let state_version = replay_record
                .block_context
                .block_number
                .checked_sub(1)
                .context("batch prover input requires a parent state version")?;
            read_state
                .state_view_at(state_version)
                .map_err(anyhow::Error::from)
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let trees = replay_records
        .iter()
        .map(|replay_record| {
            let tree_version = replay_record
                .block_context
                .block_number
                .checked_sub(1)
                .context("batch prover input requires a parent tree version")?;
            Ok(VersionedMerkleTree::new(merkle_tree.clone(), tree_version))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let batch_state = HistoricalBatchState::new(state_views, trees);

    let blocks = replay_records
        .iter()
        .map(batch_block_input)
        .collect::<Vec<_>>();

    let batch_run = generate_batch_proof_input(
        initial_proof_data,
        batch_state,
        blocks,
        da_commitment_scheme(pubdata_mode)?,
    )
    .map_err(anyhow::Error::from)?;

    let batch_output = batch_run.batch_output;
    let batch_public_input = batch_run.batch_public_input;
    let upgrade_tx_hash = b256_from_bytes32(batch_output.upgrade_tx_hash);
    let upgrade_tx_hash = (upgrade_tx_hash != B256::ZERO).then_some(upgrade_tx_hash);

    Ok(NativeBatchRunOutput {
        prover_input: batch_run.prover_input,
        pubdata: batch_run.pubdata,
        new_state_commitment: b256_from_bytes32(batch_public_input.state_after),
        da_commitment: b256_from_bytes32(batch_output.pubdata_commitment),
        number_of_layer1_txs: u256_to_u64(
            "batch_output.number_of_layer_1_txs",
            batch_output.number_of_layer_1_txs,
        )?,
        number_of_layer2_txs: u256_to_u64(
            "batch_output.number_of_layer_2_txs",
            batch_output.number_of_layer_2_txs,
        )?,
        priority_operations_hash: b256_from_bytes32(batch_output.priority_operations_hash),
        dependency_roots_rolling_hash: b256_from_bytes32(batch_output.interop_roots_rolling_hash),
        l2_to_l1_logs_root_hash: b256_from_bytes32(batch_output.l2_logs_tree_root),
        first_block_timestamp: batch_output.first_block_timestamp,
        last_block_timestamp: batch_output.last_block_timestamp,
        chain_id: u256_to_u64("batch_output.chain_id", batch_output.chain_id)?,
        sl_chain_id: u256_to_u64(
            "batch_output.settlement_layer_chain_id",
            batch_output.settlement_layer_chain_id,
        )?,
        upgrade_tx_hash,
    })
}

fn batch_block_input(replay_record: &ReplayRecord) -> BatchBlockInput<TxListSource> {
    BatchBlockInput {
        block_context: BlockMetadataFromOracle {
            chain_id: replay_record.block_context.chain_id,
            block_number: replay_record.block_context.block_number,
            block_hashes: BlockHashes(replay_record.block_context.block_hashes.0),
            timestamp: replay_record.block_context.timestamp,
            eip1559_basefee: replay_record.block_context.eip1559_basefee,
            pubdata_price: replay_record.block_context.pubdata_price,
            native_price: replay_record.block_context.native_price,
            coinbase: B160::from_be_bytes(replay_record.block_context.coinbase.into_array()),
            gas_limit: replay_record.block_context.gas_limit,
            pubdata_limit: replay_record.block_context.pubdata_limit,
            mix_hash: replay_record.block_context.mix_hash,
            blob_fee: replay_record.block_context.blob_fee,
            is_gateway: false,
        },
        tx_source: TxListSource {
            transactions: replay_record
                .transactions
                .iter()
                .cloned()
                .map(|tx| tx.encode())
                .collect::<VecDeque<_>>(),
        },
    }
}

fn da_commitment_scheme(pubdata_mode: PubdataMode) -> anyhow::Result<DACommitmentScheme> {
    (pubdata_mode.da_commitment_scheme() as u8)
        .try_into()
        .map_err(|_| anyhow::anyhow!("failed to convert DA commitment scheme"))
}

fn u256_to_u64<T>(label: &str, value: T) -> anyhow::Result<u64>
where
    T: TryInto<u64> + Copy + std::fmt::Display,
{
    value
        .try_into()
        .map_err(|_| anyhow::anyhow!("{label} does not fit into u64: {value}"))
}

fn bytes32_from_b256(value: B256) -> Bytes32 {
    Bytes32::from(value.0)
}

fn b256_from_bytes32(value: Bytes32) -> B256 {
    B256::from(value.as_u8_array())
}

#[derive(Debug)]
struct HistoricalBatchState<SV> {
    state_views: Vec<SV>,
    trees: Vec<VersionedMerkleTree>,
    cursor: usize,
}

impl<SV> HistoricalBatchState<SV> {
    fn new(state_views: Vec<SV>, trees: Vec<VersionedMerkleTree>) -> Self {
        assert_eq!(state_views.len(), trees.len());
        Self {
            state_views,
            trees,
            cursor: 0,
        }
    }
}

impl<SV: ViewState> InterfaceReadStorage for HistoricalBatchState<SV> {
    fn read(&mut self, key: B256) -> Option<B256> {
        self.state_views[self.cursor].read(key)
    }
}

impl<SV: ViewState> InterfacePreimageSource for HistoricalBatchState<SV> {
    fn get_preimage(&mut self, hash: B256) -> Option<Vec<u8>> {
        self.state_views[self.cursor].get_preimage(hash)
    }
}

impl<SV: ViewState> ReadStorageTree for HistoricalBatchState<SV> {
    fn tree_index(&mut self, key: Bytes32) -> Option<u64> {
        self.trees[self.cursor].tree_index(b256_from_bytes32(key))
    }

    fn merkle_proof(&mut self, tree_index: u64) -> LeafProof {
        self.trees[self.cursor].merkle_proof(tree_index)
    }

    fn prev_tree_index(&mut self, key: Bytes32) -> u64 {
        self.trees[self.cursor].prev_tree_index(b256_from_bytes32(key))
    }
}

impl<SV: ViewState> ForwardBatchState for HistoricalBatchState<SV> {
    fn apply_block_output(
        &mut self,
        _block_output: &zk_os_forward_system_0_4_0::run::output::BlockOutput,
    ) {
        if self.cursor + 1 < self.state_views.len() {
            self.cursor += 1;
        }
    }
}

#[derive(Debug)]
struct VersionedMerkleTree {
    inner: MerkleTree<RocksDBWrapper>,
    version: u64,
    cached_key_to_index: HashMap<B256, Option<u64>>,
    cached_missing_key_to_prev_index: HashMap<B256, u64>,
    cached_proofs: HashMap<u64, flat::StorageSlotProofEntryWithKey>,
}

impl VersionedMerkleTree {
    fn new(inner: MerkleTree<RocksDBWrapper>, version: u64) -> Self {
        Self {
            inner,
            version,
            cached_key_to_index: HashMap::new(),
            cached_missing_key_to_prev_index: HashMap::new(),
            cached_proofs: HashMap::new(),
        }
    }

    fn read(&mut self, key: B256) -> Option<B256> {
        let (proofs, _) = self
            .inner
            .prove_flat(self.version, &[key])
            .expect("failed getting Merkle proof")
            .expect("tree version disappeared");
        let proof = proofs
            .into_iter()
            .next()
            .expect("missing proof for requested key");
        let value = proof.value();
        self.cache_proof(proof);
        value
    }

    fn cache_proof(&mut self, proof: flat::StorageSlotProof) {
        match proof.proof {
            flat::InnerStorageSlotProof::Existing(entry) => {
                self.insert_proof(proof.key, entry);
            }
            flat::InnerStorageSlotProof::NonExisting {
                left_neighbor,
                right_neighbor,
            } => {
                self.cached_key_to_index.insert(proof.key, None);
                self.cached_missing_key_to_prev_index
                    .insert(proof.key, left_neighbor.inner.index);
                self.insert_proof(left_neighbor.leaf_key, left_neighbor.inner);
                self.insert_proof(right_neighbor.leaf_key, right_neighbor.inner);
            }
        }
    }

    fn insert_proof(&mut self, key: B256, proof: flat::StorageSlotProofEntry) {
        self.cached_key_to_index.insert(key, Some(proof.index));
        self.cached_proofs.insert(
            proof.index,
            flat::StorageSlotProofEntryWithKey {
                inner: proof,
                leaf_key: key,
            },
        );
    }

    fn tree_index(&mut self, key: B256) -> Option<u64> {
        if !self.cached_key_to_index.contains_key(&key) {
            self.read(key);
        }
        self.cached_key_to_index[&key]
    }

    fn merkle_proof(&mut self, tree_index: u64) -> LeafProof {
        if !self.cached_proofs.contains_key(&tree_index) {
            let proof = self
                .inner
                .prove_index_flat(self.version, tree_index)
                .expect("failed getting Merkle proof")
                .expect("tree version disappeared");
            self.cached_proofs.insert(tree_index, proof);
        }
        Self::map_proof(&self.cached_proofs[&tree_index])
    }

    fn prev_tree_index(&mut self, key: B256) -> u64 {
        if !self.cached_missing_key_to_prev_index.contains_key(&key) {
            self.read(key);
        }
        *self
            .cached_missing_key_to_prev_index
            .get(&key)
            .unwrap_or_else(|| {
                panic!(
                    "missing previous tree index for key {key:?} at version {}",
                    self.version
                )
            })
    }

    fn map_proof(proof: &flat::StorageSlotProofEntryWithKey) -> LeafProof {
        let leaf = FlatStorageLeaf {
            key: bytes32_from_b256(proof.leaf_key),
            value: bytes32_from_b256(proof.inner.value),
            next: proof.inner.next_index,
        };
        let mut merkle_path = Box::new([Bytes32::default(); usize::from(TREE_DEPTH)]);
        for (i, hash) in proof.inner.siblings.iter().enumerate() {
            merkle_path[i] = bytes32_from_b256(*hash);
        }
        for level in proof.inner.siblings.len() as u8..TREE_DEPTH {
            merkle_path[usize::from(level)] =
                bytes32_from_b256(Blake2Hasher.empty_subtree_hash(level));
        }

        LeafProof::new(proof.inner.index, leaf, merkle_path)
    }
}
