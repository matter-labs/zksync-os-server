//! Local recomputation of `state_commitment` for a sealed batch, used to verify that the batch
//! the EN replayed is the same one L1 finalized.
//!
//! `state_commitment` is the only field of `StoredBatchInfo` that is fully derivable from data we
//! already persist (merkle tree + blocks repository + replay storage) — every other field either
//! requires the transient `BlockOutput.pubdata` (for `da_commitment`) or is implied by the contract
//! checks performed on L1 itself. Comparing this single hash against the L1-side value is therefore
//! a high-signal, low-cost cross-check for replay/state divergence.

use alloy::primitives::B256;
use blake2::{Blake2s256, Digest};
use std::sync::Arc;
use thiserror::Error;
use zksync_os_merkle_tree::{Database, MerkleTree, TreeParams};
use zksync_os_storage_api::{ReadReplay, ReadRepository, RepositoryError};

#[derive(Debug, Error)]
pub enum StateCommitmentError {
    #[error("missing tree info for block {0}")]
    MissingTreeInfo(u64),
    #[error("missing block header for block {0}")]
    MissingBlockHeader(u64),
    #[error("missing replay record for block {0}")]
    MissingReplayRecord(u64),
    #[error("tree error for block {block_number}: {source}")]
    Tree {
        block_number: u64,
        #[source]
        source: anyhow::Error,
    },
    #[error("repository error for block {block_number}: {source}")]
    Repository {
        block_number: u64,
        #[source]
        source: RepositoryError,
    },
}

/// Read side of the local data needed to recompute `state_commitment`.
///
/// Hidden behind a trait so `commit_watcher` and `execute_watcher` can stay free of generics —
/// the concrete reader bundles the merkle tree, blocks repository, and replay storage.
pub trait StateCommitmentReader: Send + Sync + 'static {
    fn compute(&self, last_block_number: u64) -> Result<B256, StateCommitmentError>;
}

/// Production reader backed by the EN's persistent stores.
pub struct LocalStateCommitmentReader<DB, P, R, S>
where
    DB: Database + Send + Sync + 'static,
    P: TreeParams + Send + Sync + 'static,
    R: ReadRepository + Send + Sync + 'static,
    S: ReadReplay + Send + Sync + 'static,
{
    tree: Arc<MerkleTree<DB, P>>,
    repository: R,
    replay: S,
}

impl<DB, P, R, S> LocalStateCommitmentReader<DB, P, R, S>
where
    DB: Database + Send + Sync + 'static,
    P: TreeParams + Send + Sync + 'static,
    R: ReadRepository + Send + Sync + 'static,
    S: ReadReplay + Send + Sync + 'static,
{
    pub fn new(tree: Arc<MerkleTree<DB, P>>, repository: R, replay: S) -> Self {
        Self {
            tree,
            repository,
            replay,
        }
    }
}

impl<DB, P, R, S> StateCommitmentReader for LocalStateCommitmentReader<DB, P, R, S>
where
    DB: Database + Send + Sync + 'static,
    P: TreeParams + Send + Sync + 'static,
    R: ReadRepository + Send + Sync + 'static,
    S: ReadReplay + Send + Sync + 'static,
{
    fn compute(&self, last_block_number: u64) -> Result<B256, StateCommitmentError> {
        // Tree state at the version corresponding to the batch's last block.
        let (root_hash, leaf_count) = self
            .tree
            .root_info(last_block_number)
            .map_err(|source| StateCommitmentError::Tree {
                block_number: last_block_number,
                source,
            })?
            .ok_or(StateCommitmentError::MissingTreeInfo(last_block_number))?;

        // Block header for the last block — gives us its number, timestamp, and hash.
        let block = self
            .repository
            .get_block_by_number(last_block_number)
            .map_err(|source| StateCommitmentError::Repository {
                block_number: last_block_number,
                source,
            })?
            .ok_or(StateCommitmentError::MissingBlockHeader(last_block_number))?;
        let block_hash = block.hash();
        let block_number = block.header.number;
        let block_timestamp = block.header.timestamp;

        // Replay record provides `block_hashes` — the previous 256 block hashes baked into the
        // EVM `BLOCKHASH` window. We use entries [1..] (i.e. 255 prior hashes) and append the
        // current block's own hash to recover the 256-entry digest input.
        let replay_record = self
            .replay
            .get_replay_record(last_block_number)
            .ok_or(StateCommitmentError::MissingReplayRecord(last_block_number))?;

        let last_256_block_hashes_blake = {
            let mut hasher = Blake2s256::new();
            for prev_hash in &replay_record.block_context.block_hashes.0[1..] {
                hasher.update(prev_hash.to_be_bytes::<32>());
            }
            hasher.update(block_hash);
            hasher.finalize()
        };

        // Final state commitment, mirroring `ExtendedCommitBatchInfo::build`.
        let mut hasher = Blake2s256::new();
        hasher.update(root_hash.as_slice());
        hasher.update(leaf_count.to_be_bytes());
        hasher.update(block_number.to_be_bytes());
        hasher.update(last_256_block_hashes_blake);
        hasher.update(block_timestamp.to_be_bytes());
        Ok(B256::from_slice(&hasher.finalize()))
    }
}
