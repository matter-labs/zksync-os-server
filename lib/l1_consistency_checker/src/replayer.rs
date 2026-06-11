use anyhow::Context;
use std::ops::RangeInclusive;
use zksync_os_batch_types::ExtendedCommitBatchInfo;
use zksync_os_interface::tracing::{NopTracer, NopValidator};
use zksync_os_interface::traits::{NoopTxCallback, TxListSource};
use zksync_os_merkle_tree::{MerkleTree, RocksDBWrapper, TreeBatchOutput};
use zksync_os_multivm::run_block;
use zksync_os_storage_api::{
    OverriddenStateView, ReadReplay, ReadStateHistory, ReplayRecord, read_multichain_root,
};
use zksync_os_types::{BlockOutput, PubdataMode, ZksyncOsEncode};

/// Rebuilds batch commitments from local storage by re-executing blocks.
///
/// The pipeline discards block outputs once they are applied, but when an L1 commit needs to be
/// checked against locally replayed blocks (or a peer asks us to co-sign a batch), the outputs —
/// most notably pubdata — are needed again. Rather than caching them (pubdata alone can run into
/// hundreds of KB per batch), each block is re-executed from its persisted replay record against
/// the historical state view; everything else comes from the Merkle tree and state storage.
///
/// Rebuilding only works while the storages still cover the requested blocks: replay records,
/// state history, and tree versions are all subject to retention. Callers are expected to request
/// ranges the local pipeline has recently processed.
#[derive(Clone)]
pub struct BatchReplayer<State, Replays> {
    chain_id: u64,
    sl_chain_id: u64,
    state: State,
    replays: Replays,
    tree: MerkleTree<RocksDBWrapper>,
}

impl<State: ReadStateHistory + Clone, Replays: ReadReplay + Clone> BatchReplayer<State, Replays> {
    pub fn new(
        chain_id: u64,
        sl_chain_id: u64,
        state: State,
        replays: Replays,
        tree: MerkleTree<RocksDBWrapper>,
    ) -> Self {
        Self {
            chain_id,
            sl_chain_id,
            state,
            replays,
            tree,
        }
    }

    /// Rebuilds the commitment of the batch spanning the given block range.
    ///
    /// This is CPU-bound (a full VM run per block) and uses blocking storage I/O; call it from a
    /// blocking context (e.g. `tokio::task::spawn_blocking`).
    pub fn build_batch_info(
        &self,
        range: RangeInclusive<u64>,
        batch_number: u64,
        pubdata_mode: PubdataMode,
    ) -> anyhow::Result<ExtendedCommitBatchInfo> {
        let blocks = range
            .clone()
            .map(|block_number| self.replay_block(block_number))
            .collect::<anyhow::Result<Vec<_>>>()?;
        // The protocol version is uniform within a batch (a batch is sealed whenever it
        // changes), so the last block's record stands in for the whole batch.
        let (_, last_record) = blocks.last().context("batch block range cannot be empty")?;

        let (root_hash, leaf_count) = self
            .tree
            .root_info(*range.end())
            .context("while attempting to read the Merkle tree root")?
            .with_context(|| format!("Merkle tree version for block {} is missing", range.end()))?;
        let state_view = self
            .state
            .state_view_at(*range.end())
            .with_context(|| format!("state view at block {} is unavailable", range.end()))?;
        let multichain_root = read_multichain_root(state_view);

        let (batch_info, _) = ExtendedCommitBatchInfo::build(
            blocks
                .iter()
                .map(|(output, record)| (output, record.transactions.as_slice()))
                .collect(),
            &TreeBatchOutput {
                root_hash,
                leaf_count,
            },
            self.chain_id,
            batch_number,
            pubdata_mode,
            self.sl_chain_id,
            multichain_root,
            &last_record.protocol_version,
            &last_record.block_context.block_hashes.0,
        );
        Ok(batch_info)
    }

    /// Re-executes a single block from its persisted replay record against the state of its
    /// parent block.
    fn replay_block(&self, block_number: u64) -> anyhow::Result<(BlockOutput, ReplayRecord)> {
        let record = self
            .replays
            .get_replay_record(block_number)
            .with_context(|| format!("replay record for block {block_number} is missing"))?;
        let state_view = self
            .state
            .state_view_at(block_number - 1)
            .with_context(|| {
                format!("state view for the parent of block {block_number} is unavailable")
            })?;
        // Forced preimages are injected before execution, exactly as in the original run. We
        // intentionally skip the sequencer's post-seal step of appending them to
        // `published_preimages`: batch commitments don't read that field.
        let state_view = OverriddenStateView::with_preimages(state_view, &record.force_preimages);
        let tx_source = TxListSource {
            transactions: record
                .transactions
                .iter()
                .cloned()
                .map(ZksyncOsEncode::encode)
                .collect(),
        };
        let output = run_block(
            record.block_context,
            state_view.clone(),
            state_view,
            tx_source,
            NoopTxCallback,
            &mut NopTracer,
            &mut NopValidator,
        )
        .with_context(|| format!("while attempting to re-execute block {block_number}"))?;
        // A replay record only contains transactions that executed successfully in the original
        // run, so a rejection here means re-execution diverged from it.
        anyhow::ensure!(
            output.tx_results.iter().all(Result::is_ok),
            "re-execution of block {block_number} rejected a previously executed transaction"
        );
        Ok((output, record))
    }
}
