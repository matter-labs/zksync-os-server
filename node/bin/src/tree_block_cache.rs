use async_trait::async_trait;
use std::collections::HashMap;
use std::ops::RangeInclusive;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use zksync_os_interface::types::BlockOutput;
use zksync_os_merkle_tree::TreeBatchOutput;
use zksync_os_observability::{ComponentStateReporter, GenericComponentState};
use zksync_os_pipeline::{PeekableReceiver, PipelineComponent};
use zksync_os_storage_api::{ReplayRecord, TreeBlock};

/// Lightweight projection of a [`TreeBlock`] held in the [`TreeBlockCache`].
///
/// `TreeBlock` carries a [`zksync_os_batch_types::BlockMerkleTreeData`] whose
/// `proof` / read / write lists can be sizeable. The L1 persist watcher only
/// needs the per-block `TreeBatchOutput` plus the block's output and replay
/// record to rebuild [`zksync_os_batch_types::ExtendedCommitBatchInfo`] — so
/// the cache stores just that, avoiding cloning the proof in the hot
/// `TreeManager` → `ProverInputGenerator` path optimized by #1241.
pub struct CachedBlock {
    pub tree_output: TreeBatchOutput,
    pub block_output: BlockOutput,
    pub record: ReplayRecord,
}

/// In-memory cache of per-block data flowing through the pipeline, shared with
/// the L1 persist watcher so the watcher can recompute a batch's
/// `state_commitment` and `commitment` locally on `BlockExecution`.
#[derive(Clone, Default)]
pub struct TreeBlockCache {
    inner: Arc<Mutex<HashMap<u64, Arc<CachedBlock>>>>,
}

impl TreeBlockCache {
    pub fn insert(&self, block_number: u64, cached: Arc<CachedBlock>) {
        self.inner
            .lock()
            .expect("TreeBlockCache mutex poisoned")
            .insert(block_number, cached);
    }

    /// Returns `Some` only if every block in `block_range` is cached; otherwise
    /// `None`. The watcher treats `None` as a transient miss and logs a warn.
    pub fn get_range(&self, block_range: RangeInclusive<u64>) -> Option<Vec<Arc<CachedBlock>>> {
        let guard = self.inner.lock().expect("TreeBlockCache mutex poisoned");
        let mut out =
            Vec::with_capacity((*block_range.end() - *block_range.start() + 1) as usize);
        for block_number in block_range {
            out.push(guard.get(&block_number)?.clone());
        }
        Some(out)
    }

    /// Drops every cached entry with `block_number < threshold`. Called by the
    /// persist watcher right after a batch is written through to release memory.
    pub fn remove_lower_than(&self, threshold: u64) {
        self.inner
            .lock()
            .expect("TreeBlockCache mutex poisoned")
            .retain(|&block_number, _| block_number >= threshold);
    }
}

/// Pipeline component that snapshots every `TreeBlock`'s lightweight data into
/// `TreeBlockCache` before forwarding the block downstream. Sits between
/// `TreeManager` and whichever component consumes `TreeBlock` next
/// (`ProverInputGenerator` on the main pipeline, `BatchVerificationResponder` /
/// `NoOpSink` on the EN pipeline).
pub struct TreeBlockCacher {
    pub cache: TreeBlockCache,
}

#[async_trait]
impl PipelineComponent for TreeBlockCacher {
    type Input = TreeBlock;
    type Output = TreeBlock;

    const COMPONENT_ID: zksync_os_pipeline::ComponentId =
        zksync_os_pipeline::ComponentId::TreeBlockCacher;

    async fn run(
        self,
        mut input: PeekableReceiver<Self::Input>,
        output: mpsc::Sender<Self::Output>,
        state_reporter: ComponentStateReporter,
    ) -> anyhow::Result<()> {
        loop {
            state_reporter.enter_state(GenericComponentState::Idle);
            let Some(tree_block) = input.recv().await else {
                tracing::info!("inbound channel closed");
                return Ok(());
            };
            state_reporter.enter_state(GenericComponentState::Active);
            let block_number = tree_block.record.block_context.block_number;
            let block_timestamp = tree_block.record.block_context.timestamp;
            // Snapshot the small fields the L1 persist watcher needs without
            // touching `tree_block.tree.proof` and friends; the original block
            // is forwarded downstream unchanged.
            let cached = Arc::new(CachedBlock {
                tree_output: tree_block.tree.output,
                block_output: tree_block.output.clone(),
                record: tree_block.record.clone(),
            });
            self.cache.insert(block_number, cached);
            state_reporter.record_processed(block_number, Some(block_timestamp), None);
            if output.send(tree_block).await.is_err() {
                return Ok(());
            }
        }
    }
}
