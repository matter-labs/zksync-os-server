use alloy::primitives::BlockNumber;
use anyhow::Context;
use async_trait::async_trait;
use std::path::Path;
use std::time::Duration;
use tokio::sync::mpsc;
use vise::{Buckets, Gauge, Histogram, Metrics, Unit};
use zksync_os_batch_types::BlockMerkleTreeData;
use zksync_os_genesis::Genesis;
use zksync_os_merkle_tree::{
    MerkleTree, MerkleTreeColumnFamily, Patched, RocksDBWrapper, TreeBatchOutput, TreeEntry,
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

            let mut blocks = vec![];
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
            let range_time = TREE_METRICS.range_time.start();

            let first_block_number = blocks[0].block_number();
            if first_block_number <= last_processed_block {
                let mut tree_clone = self.tree.clone();
                tokio::task::spawn_blocking(move || {
                    tree_clone.truncate_recent_versions(first_block_number)
                })
                .await??;
            }

            let last_block_number = blocks.last().unwrap().block_number();
            let block_count = blocks.len();
            tracing::debug!(
                "Processing {block_count} tree blocks {first_block_number}..={last_block_number}"
            );

            let db_clone = self.tree.db().clone();
            let tree_blocks = tokio::task::spawn_blocking(move || {
                let patched = Patched::new(db_clone);
                let mut patched_tree = MerkleTree::new(patched)?;
                let tree_blocks = blocks
                    .into_iter()
                    .map(|block| Self::update_tree(&mut patched_tree, block))
                    .collect::<anyhow::Result<Vec<_>>>()?;

                // Single RocksDB write for all blocks.
                let flush_time = TREE_METRICS.flush_time.start();
                patched_tree.flush()?;
                let flush_time = flush_time.observe();
                tracing::debug!(?flush_time, "flushed Merkle tree updates to disk");

                anyhow::Ok(tree_blocks)
            })
            .await??;

            let range_time = range_time.observe();
            tracing::debug!(
                ?range_time,
                "processed tree blocks {first_block_number}..={last_block_number}"
            );

            // Report amortized metrics. We intentionally don't deduplicate read / written keys *across* blocks.
            TREE_METRICS
                .amortized_block_time
                .observe(range_time / block_count as u32);
            let total_writes = tree_blocks
                .iter()
                .map(|block| block.tree.written_keys.len())
                .sum::<usize>();
            let total_reads = tree_blocks
                .iter()
                .map(|block| block.tree.read_keys.len())
                .sum::<usize>();
            TREE_METRICS
                .amortized_entry_time
                .observe(range_time / total_writes.max(1) as u32);
            TREE_METRICS
                .amortized_entry_time_with_reads
                .observe(range_time / (total_reads + total_writes).max(1) as u32);

            last_processed_block = self
                .tree
                .latest_version()?
                .expect("uninitialized tree after applying blocks");
            assert_eq!(last_processed_block, last_block_number);

            // Forward each block downstream.
            for tree_block in tree_blocks {
                output.send_and_record(tree_block, &state_reporter)?;
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

    /// Updates a tree with data from a single executed block.
    ///
    /// This method uses blocking I/O.
    fn update_tree(
        patched_tree: &mut MerkleTree<Patched<RocksDBWrapper>>,
        block: AppliedBlock,
    ) -> anyhow::Result<TreeBlock> {
        let block_time = TREE_METRICS.block_time.start();
        let (block_output, read_keys) = block.output.into_parts();
        let (tree_entries, written_keys): (Vec<_>, Vec<_>) = block_output
            .storage_writes
            .iter()
            .map(|write| {
                let entry = TreeEntry {
                    key: write.key,
                    value: write.value,
                };
                (entry, write.key)
            })
            .unzip();
        let read_keys: Vec<_> = read_keys.into_iter().collect();
        let block_number = block_output.header.number;
        let write_count = written_keys.len();
        let read_count = read_keys.len();

        let (root_hash, leaf_count) =
            patched_tree.root_info(block_number - 1)?.with_context(|| {
                format!("Merkle tree missing previous block version for block {block_number}")
            })?;
        let tree_input = TreeBatchOutput {
            root_hash,
            leaf_count,
        };
        let (tree_output, update_proof) =
            patched_tree.extend_with_proof(&tree_entries, &read_keys)?;

        tracing::debug!(
            block_number = block_number,
            written_keys.len = written_keys.len(),
            read_keys.len = read_keys.len(),
            proof.sorted_leaves.len = update_proof.sorted_leaves.len(),
            proof.hashes.len = update_proof.hashes.len(),
            input = ?tree_input,
            output = ?tree_output,
            "Processed tree update"
        );

        let block_time = block_time.observe();
        TREE_METRICS
            .entry_time
            .observe(block_time / (write_count.max(1) as u32));
        TREE_METRICS
            .entry_time_with_reads
            .observe(block_time / ((write_count + read_count).max(1) as u32));
        TREE_METRICS.unique_leafs.set(tree_output.leaf_count);
        TREE_METRICS.processing_range.observe(write_count);
        TREE_METRICS.block_number.set(block_number);
        TREE_METRICS.processing_read_range.observe(read_count);
        TREE_METRICS
            .update_proof_sorted_leaves
            .observe(update_proof.sorted_leaves.len());
        TREE_METRICS
            .update_proof_hashes
            .observe(update_proof.hashes.len());
        TREE_METRICS.block_number.set(block_number);

        let tree_data = BlockMerkleTreeData {
            input: tree_input,
            output: tree_output,
            proof: update_proof,
            read_keys,
            written_keys,
        };
        Ok(TreeBlock {
            output: block_output,
            record: block.record,
            tree: tree_data,
        })
    }
}

const LATENCIES_FAST: Buckets = Buckets::exponential(0.0000001..=1.0, 2.0);
const BLOCK_RANGE_SIZE: Buckets = Buckets::exponential(1.0..=1000.0, 2.0);

#[derive(Debug, Metrics)]
#[metrics(prefix = "tree")]
pub struct TreeMetrics {
    /// Merkle tree update latency per written entry. Does not include flushing block contents to disk.
    #[metrics(unit = Unit::Seconds, buckets = LATENCIES_FAST)]
    pub entry_time: Histogram<Duration>,
    /// Merkle tree update latency per written entry. May be amortized across multiple blocks. Includes flushing to disk.
    #[metrics(unit = Unit::Seconds, buckets = LATENCIES_FAST)]
    pub amortized_entry_time: Histogram<Duration>,
    /// Merkle tree update latency per read / written entry. Does not include flushing block contents to disk.
    #[metrics(unit = Unit::Seconds, buckets = LATENCIES_FAST)]
    pub entry_time_with_reads: Histogram<Duration>,
    /// Merkle tree update latency per read / written entry. May be amortized across multiple blocks. Includes flushing to disk.
    #[metrics(unit = Unit::Seconds, buckets = LATENCIES_FAST)]
    pub amortized_entry_time_with_reads: Histogram<Duration>,
    /// Latency to process a single block in the tree. Does not include flushing block contents to disk.
    #[metrics(unit = Unit::Seconds, buckets = LATENCIES_FAST)]
    pub block_time: Histogram<Duration>,
    /// Latency to process a single block in the tree. May be amortized across multiple blocks. Includes flushing to disk.
    #[metrics(unit = Unit::Seconds, buckets = LATENCIES_FAST)]
    pub amortized_block_time: Histogram<Duration>,
    /// Latency to process a range of blocks in the tree (including flushing).
    #[metrics(unit = Unit::Seconds, buckets = LATENCIES_FAST)]
    pub range_time: Histogram<Duration>,
    /// Latency to flush tree updates to disk.
    #[metrics(unit = Unit::Seconds, buckets = LATENCIES_FAST)]
    pub flush_time: Histogram<Duration>,
    /// Number of unique leaves in the Merkle tree.
    pub unique_leafs: Gauge<u64>,
    /// Number of distinct tree entries written per block.
    #[metrics(buckets = BLOCK_RANGE_SIZE)]
    pub processing_range: Histogram<usize>,
    /// Number of distinct tree entries read (but not written) per block.
    #[metrics(buckets = BLOCK_RANGE_SIZE)]
    pub processing_read_range: Histogram<usize>,
    /// Number of sorted leaves included in the batch update proof for a single block.
    #[metrics(buckets = BLOCK_RANGE_SIZE)]
    pub update_proof_sorted_leaves: Histogram<usize>,
    /// Number of intermediate (aka sibling) hashes included in the batch update proof for a single block.
    #[metrics(buckets = BLOCK_RANGE_SIZE)]
    pub update_proof_hashes: Histogram<usize>,

    pub block_number: Gauge<BlockNumber>,
}

#[vise::register]
pub(crate) static TREE_METRICS: vise::Global<TreeMetrics> = vise::Global::new();
