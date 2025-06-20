// use crate::conversions::bytes32_to_h256;
// use crate::storage::StateHandle;
// use anyhow::Context;
// use std::ops::Div;
// use std::sync::{Arc, RwLock};
// use std::time::Duration;
// use tokio::time::Instant;
// use vise::{
//     Buckets, Counter, EncodeLabelSet, Family, Gauge, Histogram, LabeledFamily, Metrics, Unit,
// };
// use zksync_storage::rocksdb::Error;
// use zksync_storage::RocksDB;
// // use zksync_zk_os_merkle_tree::{MerkleTree, RocksDBWrapper, TreeEntry};
//
// const LATENCIES_FAST: Buckets = Buckets::exponential(0.0000001..=1.0, 2.0);
//
// const BLOCK_RANGE_SIZE: Buckets = Buckets::exponential(1.0..=1000.0, 2.0);
//
// #[derive(Debug, Metrics)]
// #[metrics(prefix = "tree")]
// pub struct TreeMetrics {
//     #[metrics(unit = Unit::Seconds, buckets = LATENCIES_FAST)]
//     pub entry_time: Histogram<Duration>,
//     #[metrics(unit = Unit::Seconds, buckets = LATENCIES_FAST)]
//     pub block_time: Histogram<Duration>,
//     #[metrics(unit = Unit::Seconds, buckets = LATENCIES_FAST)]
//     pub range_time: Histogram<Duration>,
//     #[metrics(buckets = BLOCK_RANGE_SIZE)]
//     pub processing_range: Histogram<u64>,
// }
//
// #[vise::register]
// pub(crate) static TREE_METRICS: vise::Global<TreeMetrics> = vise::Global::new();

// todo: replace with the proper TreeManager implementation (currently it only works with Postgres)
// pub struct TreeManager {
//     tree: Arc<RwLock<MerkleTree<RocksDBWrapper>>>,
//     state_handle: StateHandle,
//     last_processed_block: u64,
// }
//
//
// impl TreeManager {
//     pub fn new(
//         wrapper: RocksDBWrapper,
//         state_handle: StateHandle,
//         last_processed_block: u64,
//     ) -> Self {
//         // todo: error handling
//         let tree = MerkleTree::new(wrapper).unwrap();
//         Self {
//             tree: Arc::new(RwLock::new(tree)),
//             state_handle,
//             last_processed_block,
//         }
//     }
//
//     pub async fn run_loop(mut self) -> anyhow::Result<()> {
//         loop {
//             let last_block_to_process = self.state_handle.last_canonized_block_number();
//             if self.last_processed_block >= last_block_to_process {
//                 tokio::time::sleep(Duration::from_millis(100)).await;
//                 continue;
//             }
//
//             let started_at = Instant::now();
//             tracing::info!(
//                 "Processing {} blocks ({}-{}) in tree",
//                 last_block_to_process - self.last_processed_block,
//                 self.last_processed_block + 1,
//                 last_block_to_process
//             );
//
//             let diffs = self
//                 .state_handle
//                 .0
//                 .in_memory_storage
//                 .collect_diffs_range(self.last_processed_block + 1, last_block_to_process)
//                 .context("Failed to get diffs for block")?;
//
//             let tree_entries = diffs
//                 .into_iter()
//                 .map(|(key, value)| TreeEntry {
//                     key: bytes32_to_h256(key),
//                     value: bytes32_to_h256(key),
//                 })
//                 .collect::<Vec<_>>();
//
//             let clone = self.tree.clone();
//             let count = tree_entries.len();
//             let output =
//                 tokio::task::spawn_blocking(move || clone.write().unwrap().extend(&tree_entries))
//                     .await??;
//             tracing::info!(
//                 "Processed block {} in tree, output: {:?}",
//                 last_block_to_process,
//                 output
//             );
//
//             TREE_METRICS
//                 .entry_time
//                 .observe(started_at.elapsed().div(count as u32));
//
//             TREE_METRICS.block_time.observe(
//                 started_at.elapsed() / ((last_block_to_process - self.last_processed_block) as u32),
//             );
//
//             TREE_METRICS.range_time.observe(started_at.elapsed());
//
//             TREE_METRICS
//                 .processing_range
//                 .observe(last_block_to_process - self.last_processed_block);
//
//             self.last_processed_block = last_block_to_process;
//         }
//         Ok(())
//     }
// }
