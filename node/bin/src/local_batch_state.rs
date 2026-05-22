use crate::tree_block_cache::TreeBlockCache;
use alloy::primitives::B256;
use std::ops::RangeInclusive;
use std::sync::Arc;
use zksync_os_batch_types::ExtendedCommitBatchInfo;
use zksync_os_l1_watcher::{BatchCacheEvictor, BatchCommitmentSource};
use zksync_os_storage_api::{ReadStateHistory, read_multichain_root};
use zksync_os_types::PubdataMode;

/// [`BatchCommitmentSource`] backed by `TreeBlockCache` and state history.
///
/// Both `state_commitment` and `commitment` are computed from cached per-block
/// data plus a state view at the batch's last block. If any block in the range
/// is not cached yet (e.g., on a fresh restart before the pipeline catches
/// up), `batch_commitments` returns `Ok(None)` and the watcher skips the
/// check.
pub struct LocalBatchState<State> {
    tree_block_cache: TreeBlockCache,
    state: State,
    chain_id: u64,
    sl_chain_id: u64,
}

impl<State> LocalBatchState<State> {
    pub fn new(
        tree_block_cache: TreeBlockCache,
        state: State,
        chain_id: u64,
        sl_chain_id: u64,
    ) -> Self {
        Self {
            tree_block_cache,
            state,
            chain_id,
            sl_chain_id,
        }
    }
}

impl<State: ReadStateHistory> BatchCommitmentSource for LocalBatchState<State> {
    fn batch_commitments(
        &self,
        block_range: RangeInclusive<u64>,
        pubdata_mode: PubdataMode,
    ) -> anyhow::Result<Option<(B256, B256)>> {
        let Some(cached) = self.tree_block_cache.get_range(block_range.clone()) else {
            return Ok(None);
        };

        // Read the multichain root at the batch's last block, mirroring
        // `BatchVerificationResponder::handle_verification_request`.
        let last_block_number = *block_range.end();
        let state_view = self.state.state_view_at(last_block_number)?;
        let multichain_root = read_multichain_root(state_view);

        // Build the block triples expected by `ExtendedCommitBatchInfo::build`.
        // `tree_output` is the post-execution `TreeBatchOutput` that
        // `TreeManager` already emits per block, so no extra tree query is
        // needed here.
        let blocks_for_build: Vec<(&_, &[_], &_)> = cached
            .iter()
            .map(|cached| {
                (
                    &cached.block_output,
                    cached.record.transactions.as_slice(),
                    &cached.tree_output,
                )
            })
            .collect();

        let last_replay_record = &cached.last().expect("range is non-empty").record;
        // `batch_number` is only used to populate `CommitBatchInfo.batch_number`
        // and does not feed into `public_input_hash` or `new_state_commitment`
        // for current protocol versions — so the value passed here doesn't
        // affect the cross-check. Pass 0 to make intent explicit.
        // TODO: thread the real batch number through if a future protocol
        // version starts using it inside the public-input hash.
        let batch_number = 0u64;
        let (extended, _) = ExtendedCommitBatchInfo::build(
            blocks_for_build,
            self.chain_id,
            batch_number,
            pubdata_mode,
            self.sl_chain_id,
            multichain_root,
            &cached.first().expect("range is non-empty").record.protocol_version,
            &last_replay_record.block_context.block_hashes,
        );

        let state_commitment = extended.commit_info.new_state_commitment;
        let commitment = extended.public_input_hash();
        Ok(Some((state_commitment, commitment)))
    }
}

/// Pruner wrapper around `TreeBlockCache` that satisfies
/// [`BatchCacheEvictor`]. Separated from the read-only `LocalBatchState` so
/// the commitment source surface stays read-only.
pub struct TreeBlockCacheEvictor {
    cache: TreeBlockCache,
}

impl BatchCacheEvictor for TreeBlockCacheEvictor {
    fn evict_through(&self, last_persisted_block: u64) {
        self.cache.remove_lower_than(last_persisted_block + 1);
    }
}

/// Returns the `(commitment source, cache evictor)` pair that
/// [`zksync_os_l1_watcher::L1PersistBatchWatcher::create_watcher`] expects.
/// Both share the same `TreeBlockCache` so eviction releases the same memory
/// the reader queries.
pub fn local_batch_state_handles<State: ReadStateHistory + 'static>(
    tree_block_cache: TreeBlockCache,
    state: State,
    chain_id: u64,
    sl_chain_id: u64,
) -> (Arc<dyn BatchCommitmentSource>, Arc<dyn BatchCacheEvictor>) {
    let source: Arc<dyn BatchCommitmentSource> = Arc::new(LocalBatchState::new(
        tree_block_cache.clone(),
        state,
        chain_id,
        sl_chain_id,
    ));
    let evictor: Arc<dyn BatchCacheEvictor> = Arc::new(TreeBlockCacheEvictor {
        cache: tree_block_cache,
    });
    (source, evictor)
}
