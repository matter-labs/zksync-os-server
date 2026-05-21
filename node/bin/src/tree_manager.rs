use alloy::primitives::BlockNumber;
use async_trait::async_trait;
use std::ops::Div;
use std::path::Path;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;
use vise::{Buckets, Gauge, Histogram, Metrics, Unit};
use zksync_os_batch_types::BlockMerkleTreeData;
use zksync_os_genesis::Genesis;
use zksync_os_merkle_tree::{
    MerkleTree, MerkleTreeColumnFamily, MerkleTreeVersion, Patched, RocksDBWrapper, TreeEntry,
};
use zksync_os_observability::{ComponentStateReporter, GenericComponentState};
use zksync_os_pipeline::{HasBlockRangeEnd, PeekableReceiver, PipelineComponent, SendAndRecordExt};
use zksync_os_rocksdb::{RocksDB, RocksDBOptions, StalledWritesRetries};
use zksync_os_sequencer::model::blocks::AppliedBlock;
use zksync_os_storage_api::TreeBlock;

const MAX_BLOCKS_PER_ITERATION: usize = 32;

pub(crate) struct TreeManager {
    pub tree: MerkleTree<RocksDBWrapper>,
}

#[async_trait]
impl PipelineComponent for TreeManager {
    type Input = AppliedBlock;
    type Output = TreeBlock;

    const COMPONENT_ID: zksync_os_pipeline::ComponentId =
        zksync_os_pipeline::ComponentId::TreeManager;

    async fn run(
        self,
        mut input: PeekableReceiver<Self::Input>,
        output: mpsc::Sender<Self::Output>,
        state_reporter: ComponentStateReporter,
    ) -> anyhow::Result<()> {
        // only used to skip blocks that were already processed by the tree -
        // will be removed once idempotency is handled on the framework level
        let mut last_processed_block = self
            .tree
            .latest_version()?
            .expect("tree wasn't initialized");
        loop {
            state_reporter.enter_state(GenericComponentState::Idle);

            let mut blocks: Vec<AppliedBlock> = vec![];
            let received = input.recv_many(&mut blocks, MAX_BLOCKS_PER_ITERATION).await;
            if received == 0 {
                tracing::info!("inbound channel closed");
                return Ok(());
            }
            for block in &blocks {
                state_reporter.record_picked(
                    block.block_number(),
                    block.block_timestamp(),
                    block.batch_number(),
                );
            }

            state_reporter.enter_state(GenericComponentState::Active);
            let started_at = Instant::now();

            let first_block_number = blocks[0].output.header.number;
            if first_block_number <= last_processed_block {
                let mut tree_clone = self.tree.clone();
                tokio::task::spawn_blocking(move || {
                    tree_clone.truncate_recent_versions(first_block_number)
                })
                .await??;
            }

            let last_block_number = blocks.last().unwrap().output.header.number;
            let block_count = blocks.len();
            tracing::debug!(
                "Processing {block_count} tree blocks {first_block_number}..{last_block_number}"
            );

            let db_clone = self.tree.db().clone();
            let (blocks, outputs) = tokio::task::spawn_blocking(move || {
                let patched = Patched::new(db_clone);
                let mut patched_tree = MerkleTree::new(patched)?;
                let mut outputs = Vec::with_capacity(blocks.len());
                for block in &blocks {
                    let entries: Vec<TreeEntry> = block
                        .output
                        .storage_writes
                        .iter()
                        .map(|write| TreeEntry {
                            key: write.key,
                            value: write.value,
                        })
                        .collect();
                    outputs.push(patched_tree.extend(&entries)?);
                }
                // Single RocksDB write for all blocks.
                patched_tree.flush()?;
                Ok::<_, anyhow::Error>((blocks, outputs))
            })
            .await??;

            last_processed_block = self
                .tree
                .latest_version()?
                .expect("uninitialized tree after applying blocks");
            assert_eq!(last_processed_block, last_block_number);

            let elapsed = started_at.elapsed();
            let per_block_time = elapsed.div(block_count as u32);

            // Emit per-block metrics and forward each block downstream.
            for (block, tree_block_output) in blocks.into_iter().zip(outputs) {
                let AppliedBlock {
                    output: block_output,
                    record: replay_record,
                } = block;
                let block_number = block_output.header.number;
                let count = block_output.storage_writes.len();
                tracing::debug!(
                    "Processed {} storage writes in tree for block {}",
                    count,
                    block_number
                );

                TREE_METRICS
                    .entry_time
                    .observe(per_block_time.div(count.max(1) as u32));
                TREE_METRICS.unique_leafs.set(tree_block_output.leaf_count);
                TREE_METRICS.block_time.observe(per_block_time);
                TREE_METRICS.processing_range.observe(count.max(1) as u64);
                TREE_METRICS.block_number.set(block_number);
                tracing::debug!(
                    block_number,
                    "Processed {count} entries in tree for block {block_number}, next_free_slot={}, output: {tree_block_output:?}",
                    tree_block_output.leaf_count
                );

                let tree_block_data = BlockMerkleTreeData {
                    block_start: MerkleTreeVersion {
                        tree: self.tree.clone(),
                        block: block_number - 1,
                    },
                    block_end: MerkleTreeVersion {
                        tree: self.tree.clone(),
                        block: block_number,
                    },
                };
                output.send_and_record(
                    TreeBlock {
                        output: block_output,
                        record: replay_record,
                        tree: tree_block_data,
                    },
                    &state_reporter,
                )?;
            }
        }
    }
}

impl TreeManager {
    pub async fn load_or_initialize_tree(
        path: &Path,
        genesis: &Genesis,
    ) -> MerkleTree<RocksDBWrapper> {
        let db: RocksDB<MerkleTreeColumnFamily> = RocksDB::with_options(
            path,
            RocksDBOptions {
                block_cache_capacity: Some(128 << 20),
                include_indices_and_filters_in_block_cache: false,
                large_memtable_capacity: Some(256 << 20),
                stalled_writes_retries: StalledWritesRetries::new(Duration::from_secs(10)),
                max_open_files: None,
            },
        )
        .unwrap();

        let tree_wrapper = RocksDBWrapper::from(db);
        let mut tree = MerkleTree::new(tree_wrapper).unwrap();

        let version = tree
            .latest_version()
            .expect("cannot access tree on startup");
        if version.is_none() {
            let tree_entries = genesis
                .state()
                .await
                .storage_logs
                .iter()
                .map(|(key, value)| TreeEntry {
                    key: *key,
                    value: *value,
                })
                .collect::<Vec<_>>();
            tree.extend(&tree_entries).unwrap();
        }

        tracing::info!("Loaded tree with last processed block at {:?}", version);
        tree
    }
}

const LATENCIES_FAST: Buckets = Buckets::exponential(0.0000001..=1.0, 2.0);
const BLOCK_RANGE_SIZE: Buckets = Buckets::exponential(1.0..=1000.0, 2.0);

#[derive(Debug, Metrics)]
#[metrics(prefix = "tree")]
pub struct TreeMetrics {
    #[metrics(unit = Unit::Seconds, buckets = LATENCIES_FAST)]
    pub entry_time: Histogram<Duration>,
    #[metrics(unit = Unit::Seconds, buckets = LATENCIES_FAST)]
    pub block_time: Histogram<Duration>,
    #[metrics(unit = Unit::Seconds, buckets = LATENCIES_FAST)]
    pub range_time: Histogram<Duration>,
    pub unique_leafs: Gauge<u64>,
    #[metrics(buckets = BLOCK_RANGE_SIZE)]
    pub processing_range: Histogram<u64>,
    pub block_number: Gauge<BlockNumber>,
}

#[vise::register]
pub(crate) static TREE_METRICS: vise::Global<TreeMetrics> = vise::Global::new();
