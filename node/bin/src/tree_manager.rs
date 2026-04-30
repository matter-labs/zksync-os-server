use alloy::primitives::BlockNumber;
use anyhow::Context;
use async_trait::async_trait;
use std::ops::Div;
use std::path::Path;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;
use vise::{Buckets, Gauge, Histogram, Metrics, Unit};
use zksync_os_batch_types::BlockMerkleTreeData;
use zksync_os_genesis::Genesis;
use zksync_os_interface::types::BlockOutput;
use zksync_os_merkle_tree::{
    MerkleTree, MerkleTreeColumnFamily, RocksDBWrapper, TreeBatchOutput, TreeEntry,
};
use zksync_os_observability::{ComponentStateReporter, GenericComponentState};
use zksync_os_pipeline::{PeekableReceiver, PipelineComponent};
use zksync_os_rocksdb::{RocksDB, RocksDBOptions, StalledWritesRetries};
use zksync_os_sequencer::model::blocks::BlockOutputWithReads;

#[derive(Debug)]
pub(crate) struct TreeManager {
    pub tree: MerkleTree<RocksDBWrapper>,
}

#[async_trait]
impl PipelineComponent for TreeManager {
    type Input = (BlockOutputWithReads, zksync_os_storage_api::ReplayRecord);
    type Output = (
        BlockOutput,
        zksync_os_storage_api::ReplayRecord,
        BlockMerkleTreeData,
    );
    const NAME: &'static str = "merkle_tree";
    const OUTPUT_BUFFER_SIZE: usize = 10;

    async fn run(
        self,
        mut input: PeekableReceiver<Self::Input>,
        output: mpsc::Sender<Self::Output>,
    ) -> anyhow::Result<()> {
        let tree = self.tree;

        // only used to skip blocks that were already processed by the tree -
        // will be removed once idempotency is handled on the framework level
        let mut last_processed_block = tree.latest_version()?.expect("tree wasn't initialized");

        let latency_tracker =
            ComponentStateReporter::global().handle_for("tree", GenericComponentState::WaitingRecv);
        loop {
            latency_tracker.enter_state(GenericComponentState::WaitingRecv);

            let Some((block_output_with_reads, replay_record)) = input.recv().await else {
                tracing::info!("inbound channel closed");
                return Ok(());
            };
            let (block_output, read_keys) = block_output_with_reads.into_parts();
            latency_tracker.enter_state(GenericComponentState::Processing);
            let started_at = Instant::now();
            let block_number = block_output.header.number;

            if block_number <= last_processed_block {
                let mut tree_clone = tree.clone();
                tokio::task::spawn_blocking(move || {
                    tree_clone.truncate_recent_versions(block_number)
                })
                .await??;
            }
            tracing::debug!(
                "Processing {} storage writes in tree for block {}",
                block_output.storage_writes.len(),
                block_number
            );

            // Convert storage writes to `TreeEntry`s
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
            let read_keys_for_tree = read_keys.clone();

            let mut tree_clone = tree.clone();
            let (tree_input, (tree_output, update_proof)) = tokio::task::spawn_blocking(move || {
                let (root_hash, leaf_count) = tree_clone
                    .root_info(block_number - 1)?
                    .with_context(|| {
                        format!("Merkle tree missing previous block version for block {block_number}")
                    })?;
                let input = TreeBatchOutput { root_hash, leaf_count };
                let output = tree_clone.extend_with_proof(&tree_entries, &read_keys_for_tree)?;
                anyhow::Ok((input, output))
            })
            .await??;
            last_processed_block = tree
                .latest_version()?
                .expect("uninitialized tree after applying a block");
            assert_eq!(last_processed_block, block_number);

            tracing::info!(
                block_number = block_number,
                written_keys.len = written_keys.len(),
                read_keys.len = read_keys.len(),
                proof.sorted_leaves.len = update_proof.sorted_leaves.len(),
                proof.hashes.len = update_proof.hashes.len(),
                input = ?tree_input,
                output = ?tree_output,
                "Processed tree update"
            );

            let write_count = written_keys.len();
            let read_count = read_keys.len();
            TREE_METRICS
                .entry_time
                .observe(started_at.elapsed().div(write_count.max(1) as u32));
            TREE_METRICS.entry_time_with_reads.observe(
                started_at
                    .elapsed()
                    .div((write_count + read_count).max(1) as u32),
            );
            TREE_METRICS.unique_leafs.set(tree_output.leaf_count);
            TREE_METRICS.block_time.observe(started_at.elapsed());
            TREE_METRICS.processing_range.observe(write_count);
            TREE_METRICS.processing_read_range.observe(read_count);
            TREE_METRICS
                .update_proof_sorted_leaves
                .observe(update_proof.sorted_leaves.len());
            TREE_METRICS
                .update_proof_hashes
                .observe(update_proof.hashes.len());
            TREE_METRICS.block_number.set(block_number);

            let tree_block = BlockMerkleTreeData {
                input: tree_input,
                output: tree_output,
                proof: update_proof,
                read_keys,
                written_keys,
            };
            latency_tracker.enter_state(GenericComponentState::WaitingSend);
            output
                .send((block_output, replay_record, tree_block))
                .await?;
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
    /// Merkle tree update latency per written entry.
    #[metrics(unit = Unit::Seconds, buckets = LATENCIES_FAST)]
    pub entry_time: Histogram<Duration>,
    /// Merkle tree update latency per read / written entry.
    #[metrics(unit = Unit::Seconds, buckets = LATENCIES_FAST)]
    pub entry_time_with_reads: Histogram<Duration>,

    #[metrics(unit = Unit::Seconds, buckets = LATENCIES_FAST)]
    pub block_time: Histogram<Duration>,
    #[metrics(unit = Unit::Seconds, buckets = LATENCIES_FAST)]
    pub range_time: Histogram<Duration>,
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
