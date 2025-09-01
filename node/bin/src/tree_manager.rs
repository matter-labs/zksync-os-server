use alloy::primitives::BlockNumber;
use anyhow::Context;
use std::ops::Div;
use std::path::Path;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::mpsc::Receiver;
use tokio::sync::watch;
use tokio::time::Instant;
use vise::{Buckets, Gauge, Histogram, Metrics, Unit};
use zk_os_forward_system::run::BlockOutput;
use zksync_os_genesis::Genesis;
use zksync_os_merkle_tree::{
    MerkleTree, MerkleTreeColumnFamily, MerkleTreeForReading, RocksDBWrapper, TreeEntry,
};
use zksync_os_observability::{ComponentStateReporter, GenericComponentState};
use zksync_os_rocksdb::{RocksDB, RocksDBOptions, StalledWritesRetries};

// todo: replace with the proper TreeManager implementation (currently it only works with Postgres)
pub struct TreeManager {
    pub tree: Arc<RwLock<MerkleTree<RocksDBWrapper>>>,
    // todo: it only needs the BlockOutput - sending both for now
    block_receiver: tokio::sync::mpsc::Receiver<BlockOutput>,
    latest_block_sender: watch::Sender<u64>,
}

impl TreeManager {
    pub fn tree_wrapper(path: &Path) -> RocksDBWrapper {
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
        RocksDBWrapper::from(db)
    }

    pub fn new(
        tree_wrapper: RocksDBWrapper,
        block_receiver: Receiver<BlockOutput>,
        genesis: &Genesis,
    ) -> (TreeManager, MerkleTreeForReading<RocksDBWrapper>) {
        let (latest_block_sender, latest_block_receiver) = watch::channel(0u64);
        let mut tree = MerkleTree::new(tree_wrapper).unwrap();

        let version = tree
            .latest_version()
            .expect("cannot access tree on startup");
        if version.is_none() {
            let tree_entries = genesis
                .state()
                .storage_logs
                .iter()
                .map(|(key, value)| TreeEntry {
                    key: key.as_u8_array().into(),
                    value: value.as_u8_array().into(),
                })
                .collect::<Vec<_>>();
            tree.extend(&tree_entries).unwrap();
        }

        tracing::info!("Loaded tree with last processed block at {:?}", version);
        let tree_manager = Self {
            tree: Arc::new(RwLock::new(tree.clone())),
            block_receiver,
            latest_block_sender,
        };
        tree_manager
            .latest_block_sender
            .send(
                tree_manager
                    .last_processed_block()
                    .expect("cannot get last processed block from tree"),
            )
            .expect("cannot send last processed block to watcher");

        (
            tree_manager,
            MerkleTreeForReading::new(tree, latest_block_receiver),
        )
    }

    pub fn last_processed_block(&self) -> anyhow::Result<u64> {
        self.tree
            .write()
            .unwrap()
            .latest_version()?
            .context("tree wasn't initialized with genesis batch")
    }

    pub async fn run_loop(mut self) -> anyhow::Result<()> {
        let latency_tracker =
            ComponentStateReporter::global().handle_for("tree", GenericComponentState::WaitingRecv);
        loop {
            latency_tracker.enter_state(GenericComponentState::WaitingRecv);
            match self.block_receiver.recv().await {
                Some(block_output) => {
                    latency_tracker.enter_state(GenericComponentState::Processing);
                    let started_at = Instant::now();
                    let block_number = block_output.header.number;

                    if block_number <= self.last_processed_block()? {
                        tracing::debug!(
                            "skipping block {} as it's already processed",
                            block_number
                        );
                        continue;
                    }

                    tracing::debug!(
                        "Processing {} storage writes in tree for block {}",
                        block_output.storage_writes.len(),
                        block_number
                    );

                    // Convert StorageWrite to TreeEntry
                    let tree_entries = block_output
                        .storage_writes
                        .into_iter()
                        .map(|write| TreeEntry {
                            key: write.key.as_u8_array().into(),
                            value: write.value.as_u8_array().into(),
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

                    tracing::debug!(
                        block_number = block_number,
                        next_free_slot = output.leaf_count,
                        "Processed {} entries in tree, output: {:?}",
                        count,
                        output
                    );

                    TREE_METRICS
                        .entry_time
                        .observe(started_at.elapsed().div(count as u32));

                    TREE_METRICS.block_time.observe(started_at.elapsed());

                    TREE_METRICS.processing_range.observe(count as u64);
                    TREE_METRICS.block_number.set(block_number);
                }
                None => {
                    // Channel closed, exit the loop
                    tracing::info!("BlockOutput channel closed, exiting tree manager",);
                    break;
                }
            }
        }
        Ok(())
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
    #[metrics(buckets = BLOCK_RANGE_SIZE)]
    pub processing_range: Histogram<u64>,
    pub block_number: Gauge<BlockNumber>,
}

#[vise::register]
pub(crate) static TREE_METRICS: vise::Global<TreeMetrics> = vise::Global::new();
