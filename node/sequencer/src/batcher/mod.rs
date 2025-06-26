use crate::commitment::{CommitBatchInfo, StoredBatchInfo};
use crate::conversions::{bytes32_to_h256, tx_abi_encode};
use crate::model::{BatchJob, ReplayRecord};
use std::alloc::Global;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::mpsc::{Receiver, Sender};
use vise::{Buckets, Gauge, Histogram, LabeledFamily, Metrics, Unit};
use zk_os_basic_system::system_implementation::flat_storage_model::TestingTree;
use zk_os_forward_system::run::test_impl::{InMemoryTree, TxListSource};
use zk_os_forward_system::run::{generate_proof_input, BatchOutput, StorageCommitment};
use zksync_os_state::StateHandle;

const MAX_INFLIGHT: usize = 30;

/// This component will genarate l1 batches from the stream of blocks
/// It will also generate Prover Input for each batch.
///
/// Currently, batching is not implemented on zksync-os side, so we do 1 batch == 1 block
/// Thus, this component only generates prover input.
pub struct Batcher {
    block_sender: Receiver<(BatchOutput, ReplayRecord)>,
    // todo: this will need to be a broadcast with backpressure instead (to eth-sender and prover-api)
    batch_sender: tokio::sync::mpsc::Sender<BatchJob>,
    state_handle: StateHandle,
    bin_path: &'static str,
    num_workers: usize,
}

impl Batcher {
    pub fn new(
        // In the future it will only need ReplayRecord - it only uses BatchOutput for in-memory tree
        block_sender: tokio::sync::mpsc::Receiver<(BatchOutput, ReplayRecord)>,
        batch_sender: tokio::sync::mpsc::Sender<BatchJob>,
        state_handle: StateHandle,

        enable_logging: bool,
        num_workers: usize,
    ) -> Self {
        let bin_path = if enable_logging {
            "app_logging_enabled.bin"
        } else {
            "app.bin"
        };
        Self {
            block_sender,
            state_handle,
            batch_sender,
            bin_path,
            num_workers,
        }
    }

    /// Spawns num_workers threads and distributed work (block_source) among them
    /// todo: currently it forwards the block message to each of the workers - we could broadcast instead - no need for this forwarding then
    ///
    pub async fn run_loop(mut self) -> anyhow::Result<()> {
        let mut worker_block_senders = Vec::new();

        for worker_id in 0..self.num_workers {
            let (block_sender, block_receiver) = tokio::sync::mpsc::channel(MAX_INFLIGHT);
            worker_block_senders.push(block_sender);
            let batch_sender = self.batch_sender.clone();
            let state_handle = self.state_handle.clone();
            std::thread::spawn(move || {
                worker_loop(
                    worker_id,
                    self.num_workers,
                    block_receiver,
                    batch_sender,
                    state_handle,
                    self.bin_path,
                )
            });
        }

        loop {
            // Wait for a replay record from the channel
            // todo - just forwarding, could use broadcast instead
            match self.block_sender.recv().await {
                Some((batch_output, replay_record)) => {
                    BATCHER_METRICS
                        .current_block_number
                        .set(replay_record.context.block_number);
                    let block_number = replay_record.context.block_number;
                    tracing::debug!(
                        "Batcher dispatcher received block {} with {} transactions",
                        block_number,
                        replay_record.transactions.len()
                    );
                    for tx in worker_block_senders.iter() {
                        tx.send((batch_output.clone(), replay_record.clone()))
                            .await
                            .unwrap();
                    }
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
}

fn worker_loop(
    worker_id: usize,
    total_workers_count: usize,
    mut block_receiver: Receiver<(BatchOutput, ReplayRecord)>,
    // (block, prover input)
    batch_sender: Sender<BatchJob>,
    state_handle: StateHandle,
    bin_path: &'static str,
) {
    tracing::info!(
        worker_id = worker_id,
        "batcher prover input generation worker started"
    );

    // still need to read `block_receiver` channel async
    // although creating a Runtime here is awkward - todo
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    // thread owns its own tree - as it's not thread-safe.
    let tree = Arc::new(RwLock::new(InMemoryTree::<false> {
        storage_tree: TestingTree::new_in(Global),
        cold_storage: HashMap::new(),
    }));

    loop {
        let Some((batch_output, replay_record)) = rt.block_on(block_receiver.recv()) else {
            tracing::warn!("Worker {} block receiver channel closed", worker_id);
            break;
        };

        let bn = replay_record.context.block_number;
        tracing::debug!(worker_id = worker_id, bn = bn, "Received block");

        if bn % total_workers_count as u64 != worker_id as u64 {
            // Not my job - but still need to update the tree
            update_tree_with_batch_output(&tree, &batch_output);
            continue;
        } else {
            // my job! Computing prover input and sending result to batche_sender
            tracing::info!(
                worker_id = worker_id,
                block_number = bn,
                "starting prover input computation",
            );

            let initial_storage_commitment = StorageCommitment {
                root: *tree.read().unwrap().storage_tree.root(),
                next_free_slot: tree.read().unwrap().storage_tree.next_free_slot,
            };

            let state_view = state_handle.state_view_at_block(bn).unwrap();

            let transactions = replay_record
                .transactions
                .clone()
                .into_iter()
                .map(tx_abi_encode)
                .collect::<VecDeque<_>>();
            let list_source = TxListSource { transactions };

            let prover_input_generation_latency =
                BATCHER_METRICS.prover_input_generation[&"prover_input_generation"].start();
            let prover_input = generate_proof_input(
                PathBuf::from(bin_path),
                replay_record.context,
                initial_storage_commitment,
                tree.clone(),
                state_view,
                list_source,
            )
            .expect("proof gen failed");

            let latency = prover_input_generation_latency.observe();

            tracing::info!(
                worker_id = worker_id,
                block_number = bn,
                next_free_slot = tree.read().unwrap().storage_tree.next_free_slot,
                "Completed prover input computation in {:?}.",
                latency
            );

            update_tree_with_batch_output(&tree, &batch_output);

            let tree_output = zksync_os_merkle_tree::BatchOutput {
                root_hash: bytes32_to_h256(*tree.read().unwrap().storage_tree.root()),
                leaf_count: tree.read().unwrap().storage_tree.next_free_slot,
            };

            let commit_batch_info =
                CommitBatchInfo::new(batch_output, replay_record.transactions, tree_output);
            tracing::debug!("Expected commit batch info: {:?}", commit_batch_info);

            let stored_batch_info = StoredBatchInfo::from(commit_batch_info.clone());
            tracing::debug!("Expected stored batch info: {:?}", stored_batch_info);

            let batch = BatchJob {
                block_number: bn,
                prover_input,
                commit_batch_info,
            };
            rt.block_on(batch_sender.send(batch)).unwrap();
        }
    }
}

fn update_tree_with_batch_output(tree: &Arc<RwLock<InMemoryTree>>, batch_output: &BatchOutput) {
    // update tree - note that we need to do it for every block, not only the ones we need to process
    let mut write_tree = tree.write().unwrap();
    for w in &batch_output.storage_writes {
        write_tree.cold_storage.insert(w.key, w.value);
        write_tree.storage_tree.insert(&w.key, &w.value);
    }
    drop(write_tree);
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
