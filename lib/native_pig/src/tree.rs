//! Storage adapters over the Merkle tree shared by per-block prover input generation (node)
//! and native batch PIG. Lane-specific `ReadStorage(Tree)` trait impls live with their
//! consumers; this module works in lane-neutral `B256` terms.

use alloy::primitives::B256;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::thread;
use vise::{Buckets, Histogram, Metrics};
use zksync_os_batch_types::BlockMerkleTreeData;
use zksync_os_merkle_tree::{
    Blake2Hasher, HashTree, Leaf, MerkleTree, MerkleTreeProver, RocksDBWrapper, TreeOperation,
    api::flat,
};

pub const TREE_DEPTH: u8 = 64;

/// Lane-neutral Merkle leaf proof; consumers map it to their zksync-os version's `LeafProof`.
#[derive(Debug)]
pub struct RawLeafProof {
    pub index: u64,
    pub key: B256,
    pub value: B256,
    pub next_index: u64,
    pub path: Box<[B256; TREE_DEPTH as usize]>,
}

/// Error returned by [`TreeOutputAdapter`] when it contains no queried data.
#[derive(Debug)]
struct NoData;

/// Very fast adapter based on the data contained in a Merkle tree batch update proof.
/// It requires zero I/O for all operations, but doesn't work for storage slots not read /
/// written during block execution.
#[derive(Debug)]
struct TreeOutputAdapter {
    leaf_count_before_update: u64,
    sorted_leaves: BTreeMap<u64, Leaf>,
    key_to_op: HashMap<B256, TreeOperation>,
    sibling_hashes: HashMap<(u8, u64), B256>,
    // Fields below are only used for metrics and can be removed in the future.
    queried_proofs: HashSet<u64>,
}

impl Drop for TreeOutputAdapter {
    fn drop(&mut self) {
        const DEBUG_COUNT: usize = 5;

        if thread::panicking() {
            return; // Do not report potentially incomplete data if generating prover input failed
        }

        let expected_queried_proofs: HashSet<_> = self.sorted_leaves.keys().copied().collect();
        let unexpected_queried_proofs: Vec<_> = self
            .queried_proofs
            .difference(&expected_queried_proofs)
            .take(DEBUG_COUNT)
            .collect();
        if !unexpected_queried_proofs.is_empty() {
            tracing::warn!(
                ?unexpected_queried_proofs,
                "prover queried unexpected Merkle proofs"
            );
        }

        let not_queried_proofs: Vec<_> = expected_queried_proofs
            .difference(&self.queried_proofs)
            .take(DEBUG_COUNT)
            .collect();
        if !not_queried_proofs.is_empty() {
            tracing::warn!(
                ?not_queried_proofs,
                "prover didn't query expected Merkle proofs"
            );
        }
    }
}

impl TreeOutputAdapter {
    fn new(tree_data: BlockMerkleTreeData) -> Self {
        let key_to_index: HashMap<_, _> = tree_data.keys_and_ops().collect();
        // An empty proof (block with no tree operations) has no sibling hashes to extract;
        // `sibling_hashes` cannot handle it. Every query then goes to the fallback.
        let sibling_hashes = if tree_data.proof.sorted_leaves.is_empty() {
            HashMap::new()
        } else {
            tree_data
                .proof
                .sibling_hashes(TREE_DEPTH, tree_data.input.leaf_count)
                .map(|(location, hash)| (location, hash.0.into()))
                .collect()
        };

        Self {
            queried_proofs: HashSet::with_capacity(tree_data.proof.sorted_leaves.len()),
            leaf_count_before_update: tree_data.input.leaf_count,
            sorted_leaves: tree_data.proof.sorted_leaves,
            key_to_op: key_to_index,
            sibling_hashes,
        }
    }

    fn merkle_path(&self, tree_index: u64) -> Box<[B256; TREE_DEPTH as usize]> {
        let mut path = [B256::default(); TREE_DEPTH as usize];
        let mut idx_on_level = tree_index;
        let mut last_idx_on_level = self.leaf_count_before_update - 1;
        for (depth, sibling_hash) in (0..TREE_DEPTH).zip(&mut path) {
            let sibling_location = (depth, idx_on_level ^ 1);
            let hash = self.sibling_hashes.get(&sibling_location).copied();
            let hash = hash.unwrap_or_else(|| {
                // The hash may only be missing for the last index on the level.
                assert!(
                    sibling_location.1 > last_idx_on_level,
                    "missing Merkle path for index {tree_index} at {sibling_location:?}"
                );
                Blake2Hasher.empty_subtree_hash(depth)
            });
            *sibling_hash = hash;

            idx_on_level /= 2;
            last_idx_on_level /= 2;
        }
        Box::new(path)
    }

    fn read(&mut self, key: B256) -> Result<Option<B256>, NoData> {
        let op = self.key_to_op.get(&key).ok_or(NoData)?;
        Ok(match op {
            TreeOperation::Hit { index } => {
                let leaf = self.sorted_leaves.get(index).ok_or(NoData)?;
                Some(leaf.value)
            }
            TreeOperation::Miss { .. } => None,
        })
    }

    fn tree_index(&mut self, key: B256) -> Result<Option<u64>, NoData> {
        let op = self.key_to_op.get(&key).ok_or(NoData)?;
        Ok(match op {
            TreeOperation::Hit { index } => Some(*index),
            TreeOperation::Miss { .. } => None,
        })
    }

    fn merkle_proof(&mut self, tree_index: u64) -> Result<RawLeafProof, NoData> {
        self.queried_proofs.insert(tree_index);

        let leaf = self.sorted_leaves.get(&tree_index).ok_or(NoData)?;
        Ok(RawLeafProof {
            index: tree_index,
            key: leaf.key,
            value: leaf.value,
            next_index: leaf.next_index,
            path: self.merkle_path(tree_index),
        })
    }

    fn prev_tree_index(&mut self, key: B256) -> Result<u64, NoData> {
        let op = self.key_to_op.get(&key).ok_or(NoData)?;
        if let TreeOperation::Miss { prev_index } = op {
            Ok(*prev_index)
        } else {
            Err(NoData)
        }
    }
}

/// Storage adapter that reads data from the Merkle tree. This adapter is very inefficient in
/// terms of I/O, but is universal as opposed to using a batch update proof (which will miss data
/// for any keys not read / written in the batch).
#[derive(Debug)]
pub struct VersionedMerkleTree {
    inner: MerkleTree<RocksDBWrapper>,
    version: u64,
    cached_key_to_index: HashMap<B256, Option<u64>>,
    cached_missing_key_to_prev_index: HashMap<B256, u64>,
    cached_proofs: HashMap<u64, flat::StorageSlotProofEntryWithKey>,
}

impl VersionedMerkleTree {
    pub fn new(inner: MerkleTree<RocksDBWrapper>, version: u64) -> Self {
        Self {
            inner,
            version,
            cached_key_to_index: HashMap::new(),
            cached_missing_key_to_prev_index: HashMap::new(),
            cached_proofs: HashMap::new(),
        }
    }

    pub fn read(&mut self, key: B256) -> Option<B256> {
        let (proof, _) = self
            .inner
            .prove_flat(self.version, &[key])
            .expect("failed getting Merkle proof")
            .expect("tree version disappeared");
        assert_eq!(
            proof.len(),
            1,
            "sanity check failed: unexpected proof length"
        );
        let proof = proof.into_iter().next().unwrap();
        let value = proof.value();

        // Cache the proof since it's guaranteed to be requested later.
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

    pub fn tree_index(&mut self, key: B256) -> Option<u64> {
        if !self.cached_key_to_index.contains_key(&key) {
            // Use proof API to get the necessary data. This is inefficient, but should (almost)
            // never be triggered in practice.
            self.read(key);
        }
        self.cached_key_to_index[&key]
    }

    pub fn merkle_proof(&mut self, tree_index: u64) -> RawLeafProof {
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

    pub fn prev_tree_index(&mut self, key: B256) -> u64 {
        if !self.cached_missing_key_to_prev_index.contains_key(&key) {
            assert_eq!(self.read(key), None);
        }
        self.cached_missing_key_to_prev_index[&key]
    }

    fn map_proof(proof: &flat::StorageSlotProofEntryWithKey) -> RawLeafProof {
        let mut path = Box::new([B256::default(); TREE_DEPTH as usize]);
        for (i, hash) in proof.inner.siblings.iter().enumerate() {
            path[i] = *hash;
        }
        // Fill in remaining Merkle path hashes from empty subtree hashes.
        let merkle_path_len = proof.inner.siblings.len() as u8;
        for level in merkle_path_len..TREE_DEPTH {
            path[usize::from(level)] = Blake2Hasher.empty_subtree_hash(level);
        }

        RawLeafProof {
            index: proof.inner.index,
            key: proof.leaf_key,
            value: proof.inner.value,
            next_index: proof.inner.next_index,
            path,
        }
    }
}

/// Reports storage-related metrics on drop.
impl Drop for VersionedMerkleTree {
    fn drop(&mut self) {
        if thread::panicking() {
            return; // Do not report potentially incomplete data if generating prover input failed
        }

        TREE_ADAPTER_METRICS
            .unexpected_queried_keys
            .observe(self.cached_key_to_index.len());
        TREE_ADAPTER_METRICS
            .unexpected_queried_missing_keys
            .observe(self.cached_missing_key_to_prev_index.len());
        TREE_ADAPTER_METRICS
            .unexpected_queried_proofs
            .observe(self.cached_proofs.len());

        tracing::info!(
            version = self.version,
            cached_key_to_index.len = self.cached_key_to_index.len(),
            cached_missing_key_to_prev_index.len = self.cached_missing_key_to_prev_index.len(),
            cached_proofs.len = self.cached_proofs.len(),
            "finished providing storage via Merkle tree"
        );
    }
}

/// Efficient tree adapter that takes most data from the block's batch update proof and falls
/// back to [`VersionedMerkleTree`]. During normal execution, the fallback shouldn't be used
/// at all.
#[derive(Debug)]
pub struct EfficientTreeAdapter {
    main: TreeOutputAdapter,
    fallback: VersionedMerkleTree,
}

impl EfficientTreeAdapter {
    pub fn new(tree_data: BlockMerkleTreeData, fallback: VersionedMerkleTree) -> Self {
        Self {
            main: TreeOutputAdapter::new(tree_data),
            fallback,
        }
    }

    pub fn read(&mut self, key: B256) -> Option<B256> {
        self.main
            .read(key)
            .unwrap_or_else(|_| self.fallback.read(key))
    }

    pub fn tree_index(&mut self, key: B256) -> Option<u64> {
        self.main
            .tree_index(key)
            .unwrap_or_else(|_| self.fallback.tree_index(key))
    }

    pub fn merkle_proof(&mut self, tree_index: u64) -> RawLeafProof {
        self.main
            .merkle_proof(tree_index)
            .unwrap_or_else(|_| self.fallback.merkle_proof(tree_index))
    }

    pub fn prev_tree_index(&mut self, key: B256) -> u64 {
        self.main
            .prev_tree_index(key)
            .unwrap_or_else(|_| self.fallback.prev_tree_index(key))
    }
}

const LEN_BUCKETS: Buckets = Buckets::exponential(1.0..=1000.0, 2.0);

#[derive(Debug, Metrics)]
#[metrics(prefix = "prover_input_generator")]
struct TreeAdapterMetrics {
    /// Number of unexpected existing storage slots queried per block. Positive values are abnormal.
    #[metrics(buckets = LEN_BUCKETS)]
    unexpected_queried_keys: Histogram<usize>,
    /// Number of unexpected missing storage slots queried per block. Positive values are abnormal.
    #[metrics(buckets = LEN_BUCKETS)]
    unexpected_queried_missing_keys: Histogram<usize>,
    /// Number of unexpected Merkle proofs queried per block. Positive values are abnormal.
    #[metrics(buckets = LEN_BUCKETS)]
    unexpected_queried_proofs: Histogram<usize>,
}

#[vise::register]
static TREE_ADAPTER_METRICS: vise::Global<TreeAdapterMetrics> = vise::Global::new();
