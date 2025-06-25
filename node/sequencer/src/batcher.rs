use crate::commitment::CommitBatchInfo;
use crate::conversions::{bytes32_to_h256, tx_abi_encode};
use crate::model::ReplayRecord;
use std::alloc::Global;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::mpsc::Receiver;
use tokio::sync::{mpsc, watch};
use tokio::task::spawn_blocking;
use vise::{Buckets, Gauge, Histogram, LabeledFamily, Metrics, Unit};
use zk_os_basic_system::system_implementation::flat_storage_model::TestingTree;
use zk_os_forward_system::run::test_impl::{InMemoryTree, TxListSource};
use zk_os_forward_system::run::{generate_proof_input, BatchOutput, StorageCommitment};
use zksync_os_merkle_tree::{MerkleTreeReader, RocksDBWrapper};
use zksync_os_state::StateHandle;

/// This component will genearte l1 batches from the stream of blocks
/// It will also generate Prover Input for each batch.
///
/// Currently, batching is not implemented on zksync-os side, so we do 1 batch == 1 block
///
pub struct Batcher {
    block_receiver: mpsc::Receiver<(BatchOutput, ReplayRecord)>,
    // block number that persistent tree has processed
    tree_block_watch: watch::Receiver<u64>,
    state_handle: StateHandle,

    // using in-memory tree for now - until persistent tree implements needed interface
    // todo: clean a mess with Arc<RwLock>
    in_memory_tree: Arc<RwLock<InMemoryTree>>,

    bin_path: &'static str,
}

impl Batcher {
    pub fn new(
        // In the future it will only need ReplayRecord - it onlt uses BatchOutput for in-memory tree
        block_receiver: Receiver<(BatchOutput, ReplayRecord)>,
        tree_block_watch: watch::Receiver<u64>,

        state_handle: StateHandle,

        // todo: this is not used currently as we don't have impl `ReadStorageTree` for it
        _tree: MerkleTreeReader<RocksDBWrapper>,

        enable_logging: bool,
    ) -> Self {
        let in_memory_tree = InMemoryTree {
            storage_tree: TestingTree::new_in(Global),
            cold_storage: HashMap::new(),
        };
        let bin_path = if enable_logging {
            "app_logging_enabled.bin"
        } else {
            "app.bin"
        };
        Self {
            block_receiver,
            tree_block_watch,
            state_handle,
            in_memory_tree: Arc::new(RwLock::new(in_memory_tree)),
            bin_path,
        }
    }

    pub async fn run_loop(mut self) -> anyhow::Result<()> {
        loop {
            // Wait for a replay record from the channel
            match self.block_receiver.recv().await {
                Some((batch_output, replay_record)) => {
                    BATCHER_METRICS
                        .current_block_number
                        .set(replay_record.context.block_number);
                    let total_latency = BATCHER_METRICS.prover_input_generation[&"total"].start();
                    let prepare_latency =
                        BATCHER_METRICS.prover_input_generation[&"prepare"].start();
                    let block_number = replay_record.context.block_number;

                    tracing::info!(
                        "Batcher received block {} with {} transactions",
                        block_number,
                        replay_record.transactions.len()
                    );

                    // Wait for the persistent tree to process this block
                    // note that we don't use it - but still wait to ensure it's not falling behind
                    self.wait_for_tree_block(block_number).await?;

                    let state_view = self.state_handle.state_view_at_block(block_number)?;

                    let in_memory_storage_commitment = StorageCommitment {
                        root: *self.in_memory_tree.read().unwrap().storage_tree.root(),
                        next_free_slot: self
                            .in_memory_tree
                            .read()
                            .unwrap()
                            .storage_tree
                            .next_free_slot,
                    };
                    // let persistent_storage_commitment = self.tree.root_info(block_number - 1)?;

                    // assert_eq!(
                    //     in_memory_storage_commitment.root,
                    //     h256_to_bytes32(persistent_storage_commitment.unwrap().0)
                    // );
                    // assert_eq!(
                    //     in_memory_storage_commitment.next_free_slot,
                    //     persistent_storage_commitment.unwrap().1
                    // );

                    let transactions = replay_record
                        .transactions
                        .clone()
                        .into_iter()
                        .map(tx_abi_encode)
                        .collect::<VecDeque<_>>();

                    let list_source = TxListSource { transactions };

                    prepare_latency.observe();
                    let prover_input_generation_latency =
                        BATCHER_METRICS.prover_input_generation[&"prover_input_generation"].start();

                    // let tree_view = TreeView::new(self.tree.clone(), block_number);
                    // let in_memory_tree_clone = self.in_memory_tree.clone();
                    let cloned_tree = self.in_memory_tree.clone();
                    let prover_input = spawn_blocking(move || {
                        generate_proof_input(
                            PathBuf::from(self.bin_path),
                            replay_record.context,
                            in_memory_storage_commitment,
                            cloned_tree,
                            state_view,
                            list_source,
                        )
                    })
                    .await?
                    .map_err(|err| anyhow::anyhow!("{}", err.0).context("zk_ee internal error"))?;

                    prover_input_generation_latency.observe();

                    let memory_tree_latency =
                        BATCHER_METRICS.prover_input_generation[&"update_memory_tree"].start();

                    let mut write_tree = self.in_memory_tree.write().unwrap();
                    for write in batch_output.storage_writes.iter() {
                        write_tree.cold_storage.insert(write.key, write.value);
                        write_tree.storage_tree.insert(&write.key, &write.value);
                    }
                    drop(write_tree);

                    memory_tree_latency.observe();
                    let total_latency = total_latency.observe();
                    let tree_output = zksync_os_merkle_tree::BatchOutput {
                        root_hash: bytes32_to_h256(
                            *self.in_memory_tree.read().unwrap().storage_tree.root(),
                        ),
                        leaf_count: self
                            .in_memory_tree
                            .read()
                            .unwrap()
                            .storage_tree
                            .next_free_slot,
                    };
                    let commit_batch_info =
                        CommitBatchInfo::new(batch_output, replay_record.transactions, tree_output);

                    tracing::debug!("Expected commit batch info: {:?}", commit_batch_info);

                    tracing::info!(
                        "Prover input generated for block {}. length: {}. Took {:?}",
                        block_number,
                        prover_input.len(),
                        total_latency
                    );

                    //
                    // let u8s_prover_input: Vec<u8> = prover_input
                    //     .into_iter()
                    //     .flat_map(|x| x.to_le_bytes())
                    //     .collect();
                    //
                    // let base64 = base64::encode(&u8s_prover_input);
                    // fs::write(format!("prover_input_{}.txt", block_number), base64)?;
                }
                None => {
                    // Channel closed, exit the loop
                    tracing::info!("Block replay channel closed, exiting batcher",);
                    break;
                }
            }
        }
        Ok(())
    }

    async fn wait_for_tree_block(&mut self, target_block: u64) -> anyhow::Result<()> {
        loop {
            let current_tree_block = *self.tree_block_watch.borrow();

            if current_tree_block >= target_block {
                // tracing::info!(
                //     "Tree has processed block {} (current: {})",
                //     target_block,
                //     current_tree_block
                // );
                return Ok(());
            }

            // Wait for the next update from the tree
            self.tree_block_watch.changed().await?;
        }
    }
}

const LATENCIES_FAST: Buckets = Buckets::exponential(0.0000001..=1.0, 2.0);
#[derive(Debug, Metrics)]
#[metrics(prefix = "batcher")]
pub struct BatcherMetrics {
    #[metrics(unit = Unit::Seconds, labels = ["stage"], buckets = LATENCIES_FAST)]
    pub prover_input_generation: LabeledFamily<&'static str, Histogram<Duration>>,

    pub current_block_number: Gauge<u64>,
}

#[vise::register]
pub(crate) static BATCHER_METRICS: vise::Global<BatcherMetrics> = vise::Global::new();
