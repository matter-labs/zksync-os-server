//! DB-at-version storage tree used to build prover input.
//!
//! [`VersionedMerkleTree`] reads a persistent Merkle tree at one fixed version.
//! It authenticates each storage slot against that version with the tree
//! `prove_flat` / `prove_index_flat` plumbing. The native Airbender lane and the
//! second-proof (ZiSK) lane both build their witnesses on top of it.

use alloy::primitives::B256;
use std::collections::HashMap;
use std::thread;
use vise::{Buckets, Histogram, Metrics};
use zk_ee::utils::Bytes32;
use zk_os_basic_system::system_implementation::flat_storage_model::FlatStorageLeaf;
use zk_os_forward_system::run::LeafProof;
use zksync_os_merkle_tree::{
    Blake2Hasher, HashTree, MerkleTree, MerkleTreeProver, RocksDBWrapper, api::flat,
};

const TREE_DEPTH: u8 = 64;

/// Storage adapter that reads data from the Merkle tree. This adapter is very inefficient in terms of I/O,
/// but is universal as opposed to using a batch update proof (which will miss data for any keys
/// not read / written in the batch).
///
/// The second-proof (ZiSK) lane reuses this adapter for its per-slot proof
/// extraction, so it authenticates every slot against the versioned tree with
/// the same `prove_flat` / `prove_index_flat` plumbing the native lane uses.
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

    /// Root hash and leaf count at this tree version. The version is always
    /// present. Some version was loaded into the tree before this adapter was
    /// built.
    pub fn root_info(&self) -> anyhow::Result<(B256, u64)> {
        self.inner
            .root_info(self.version)?
            .ok_or_else(|| anyhow::anyhow!("tree version {} is missing", self.version))
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
            // Use proof API to get the necessary data. This is inefficient, but should (almost) never
            // be triggered in practice.
            self.read(key);
        }
        self.cached_key_to_index[&key]
    }

    pub fn merkle_proof(&mut self, tree_index: u64) -> LeafProof {
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

    fn map_proof(proof: &flat::StorageSlotProofEntryWithKey) -> LeafProof {
        let leaf = FlatStorageLeaf {
            key: proof.leaf_key.0.into(),
            value: proof.inner.value.0.into(),
            next: proof.inner.next_index,
        };

        let mut merkle_path = Box::new([Bytes32::default(); 64]);
        for (i, hash) in proof.inner.siblings.iter().enumerate() {
            merkle_path[i] = hash.0.into();
        }
        // Fill in remaining Merkle path hashes from empty subtree hashes.
        let merkle_path_len = proof.inner.siblings.len() as u8;
        for level in merkle_path_len..TREE_DEPTH {
            merkle_path[usize::from(level)] = Blake2Hasher.empty_subtree_hash(level).0.into();
        }

        LeafProof::new(proof.inner.index, leaf, merkle_path)
    }

    pub fn prev_tree_index(&mut self, key: B256) -> u64 {
        if !self.cached_missing_key_to_prev_index.contains_key(&key) {
            assert_eq!(self.read(key), None);
        }
        self.cached_missing_key_to_prev_index[&key]
    }
}

/// Reports storage-related metrics on drop.
impl Drop for VersionedMerkleTree {
    fn drop(&mut self) {
        if thread::panicking() {
            return; // Do not report potentially incomplete data if generating prover input failed
        }

        VERSIONED_MERKLE_TREE_METRICS
            .unexpected_queried_keys
            .observe(self.cached_key_to_index.len());
        VERSIONED_MERKLE_TREE_METRICS
            .unexpected_queried_missing_keys
            .observe(self.cached_missing_key_to_prev_index.len());
        VERSIONED_MERKLE_TREE_METRICS
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

const LEN_BUCKETS: Buckets = Buckets::exponential(1.0..=1000.0, 2.0);

// The metric names keep the `prover_input_generator` prefix so the moved tree
// keeps the exact time series it had while it lived in `node/bin`.
#[derive(Debug, Metrics)]
#[metrics(prefix = "prover_input_generator")]
struct VersionedMerkleTreeMetrics {
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
static VERSIONED_MERKLE_TREE_METRICS: vise::Global<VersionedMerkleTreeMetrics> =
    vise::Global::new();
