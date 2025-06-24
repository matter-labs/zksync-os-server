use crate::conversions::bytes32_to_h256;
use anyhow::Context;
use std::ops::Div;
use std::path::Path;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::Instant;
use vise::{Buckets, Histogram, Metrics, Unit};
use zk_os_forward_system::run::BatchOutput;
use zksync_os_merkle_tree::{MerkleTree, MerkleTreeColumnFamily, RocksDBWrapper, TreeEntry};
use zksync_storage::{RocksDB, RocksDBOptions, StalledWritesRetries};

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
    #[metrics(buckets = BLOCK_RANGE_SIZE)]
    pub processing_range: Histogram<u64>,
}

#[vise::register]
pub(crate) static TREE_METRICS: vise::Global<TreeMetrics> = vise::Global::new();

// todo: replace with the proper TreeManager implementation (currently it only works with Postgres)
pub struct TreeManager {
    tree: Arc<RwLock<MerkleTree<RocksDBWrapper>>>,
    receiver: tokio::sync::mpsc::Receiver<BatchOutput>,
    latest_block_sender: watch::Sender<u64>,
}

impl TreeManager {
    pub fn new(path: &Path, receiver: tokio::sync::mpsc::Receiver<BatchOutput>) -> (Self, watch::Receiver<u64>) {
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
            // Initialize the tree with an empty genesis batch
            tree.extend(&[]).expect("cannot extend tree on startup");
        }

        tracing::info!("Loaded tree with last processed block at {:?}", version);

        let initial_block = version.unwrap_or(0);
        let (latest_block_sender, latest_block_receiver) = watch::channel(initial_block);

        let tree_manager = Self {
            tree: Arc::new(RwLock::new(tree)),
            receiver,
            latest_block_sender,
        };

        (tree_manager, latest_block_receiver)
    }

    pub fn last_processed_block(&self) -> anyhow::Result<u64> {
        self.tree
            .write()
            .unwrap()
            .latest_version()?
            .context("tree wasn't initialized with genesis batch")
    }

    pub async fn run_loop(mut self) -> anyhow::Result<()> {
        loop {
            // Wait for messages from the channel
            match self.receiver.recv().await {
                Some(batch_output) => {
                    let started_at = Instant::now();
                    let block_number = batch_output.header.number;

                    if block_number <= self.last_processed_block()? {
                        tracing::debug!(
                            "skipping block {} as it's already processed",
                            block_number
                        );
                        continue;
                    }

                    tracing::info!(
                        "Processing {} storage writes in tree for block {}",
                        batch_output.storage_writes.len(),
                        block_number
                    );

                    // Convert StorageWrite to TreeEntry
                    let tree_entries = batch_output
                        .storage_writes
                        .into_iter()
                        .map(|write| TreeEntry {
                            key: bytes32_to_h256(write.key),
                            value: bytes32_to_h256(write.value),
                        })
                        .collect::<Vec<_>>();

                    let clone = self.tree.clone();
                    let count = tree_entries.len();
                    let output = tokio::task::spawn_blocking(move || {
                        clone.write().unwrap().extend(&tree_entries)
                    })
                    .await??;
                    let version_after = self.tree.write().unwrap().latest_version()?;

                    assert_eq!(version_after, Some(block_number));

                    // Send the latest processed block number to watchers
                    let _ = self.latest_block_sender.send(block_number);

                    tracing::info!(
                        block_number = block_number,
                        "Processed {} entries in tree, output: {:?}",
                        count,
                        output
                    );

                    TREE_METRICS
                        .entry_time
                        .observe(started_at.elapsed().div(count as u32));

                    TREE_METRICS
                        .block_time
                        .observe(started_at.elapsed());

                    TREE_METRICS.processing_range.observe(count as u64);
                }
                None => {
                    // Channel closed, exit the loop
                    tracing::info!("BatchOutput channel closed, exiting tree manager");
                    break;
                }
            }
        }
        Ok(())
    }
}
