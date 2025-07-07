use crate::model::{BatchJob, ReplayRecord};
use crate::CHAIN_ID;
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
use zksync_os_l1_sender::commitment::{CommitBatchInfo, StoredBatchInfo};
use zksync_os_l1_sender::L1SenderHandle;
use zksync_os_state::StateHandle;
use zksync_os_types::EncodableZksyncOs;

// used in two places:
// * number of blocks received by Batcher but not distributed to Workers yet
// * number of blocks processed by Workers, but not sent to l1 sender yet because of gaps (we need to send in order)
const MAX_INFLIGHT: usize = 30;

/// This component will genarate l1 batches from the stream of blocks
/// It will also generate Prover Input for each batch.
///
/// Currently, batching is not implemented on zksync-os side, so we do 1 batch == 1 block
/// Thus, this component only generates prover input.
/// Additionally, it doesn't use the Persistent Merkle Tree yet -
/// so we spawn multiple workers with in-memory tree in each.
/// The component will be heavily reworked once we migrate to the Peristent Tree
pub struct Batcher {
    block_sender: Receiver<(BatchOutput, ReplayRecord)>,
    // todo: the following two may just need to be a broadcast with backpressure instead (to eth-sender and prover-api)
    batch_sender: tokio::sync::mpsc::Sender<BatchJob>,
    // handled by l1-sender. We ensure that they are sent in order.
    commit_batch_info_sender: Option<L1SenderHandle>,
    state_handle: StateHandle,
    bin_path: &'static str,
    num_workers: usize,
}

impl Batcher {
    pub fn new(
        // In the future it will only need ReplayRecord - it only uses BatchOutput for in-memory tree
        block_sender: tokio::sync::mpsc::Receiver<(BatchOutput, ReplayRecord)>,
        batch_sender: tokio::sync::mpsc::Sender<BatchJob>,
        // handled by l1-sender
        commit_batch_info_sender: Option<L1SenderHandle>,
        state_handle: StateHandle,

        enable_logging: bool,
        num_workers: usize,
    ) -> Self {
        let bin_path = if enable_logging {
            "../app_logging_enabled.bin"
        } else {
            "../app.bin"
        };
        Self {
            block_sender,
            state_handle,
            batch_sender,
            commit_batch_info_sender,
            bin_path,
            num_workers,
        }
    }

    /// Spawns num_workers threads and distributed work (block_source) among them
    /// todo: currently it forwards the block message to each of the workers - we could broadcast instead - no need for this forwarding then
    ///
    pub async fn run_loop(mut self) -> anyhow::Result<()> {
        let mut worker_block_senders = Vec::new();

        // todo: will be reworked when migrating batcher to the persistent tree
        let commit_tx = self
            .commit_batch_info_sender
            .map(|commit_batch_info_sender| {
                let (commit_tx, commit_rx) = tokio::sync::mpsc::channel(MAX_INFLIGHT);
                // CommitBatchInfo proxy that ensures order
                tokio::spawn(async move {
                    if let Err(e) = ordered_committer(commit_rx, commit_batch_info_sender).await {
                        tracing::error!("ordered_committer exited: {e:?}");
                    }
                });
                commit_tx
            });

        for worker_id in 0..self.num_workers {
            let (block_sender, block_receiver) = tokio::sync::mpsc::channel(MAX_INFLIGHT);

            worker_block_senders.push(block_sender);
            let batch_sender = self.batch_sender.clone();
            let state_handle = self.state_handle.clone();
            let commit_tx = commit_tx.clone();
            std::thread::spawn(move || {
                worker_loop(
                    worker_id,
                    self.num_workers,
                    block_receiver,
                    batch_sender,
                    commit_tx,
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
                        .set(replay_record.block_context.block_number);
                    let block_number = replay_record.block_context.block_number;
                    tracing::debug!(
                        "Batcher dispatcher received block {} with {} L1 transactions and {} L2 transactions",
                        block_number,
                        replay_record.l1_transactions.len(),
                        replay_record.l2_transactions.len()
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
    batch_sender: Sender<BatchJob>,
    commit_batch_info_sender: Option<Sender<CommitBatchInfo>>,
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

        let bn = replay_record.block_context.block_number;
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

            let l1_transactions = replay_record
                .l1_transactions
                .clone()
                .into_iter()
                .map(|l1_tx| l1_tx.encode_zksync_os());
            let l2_transactions = replay_record
                .l2_transactions
                .into_iter()
                .map(|l2_tx| l2_tx.encode_zksync_os());
            let transactions = l1_transactions
                .chain(l2_transactions)
                .collect::<VecDeque<_>>();
            let list_source = TxListSource { transactions };

            let prover_input_generation_latency =
                BATCHER_METRICS.prover_input_generation[&"prover_input_generation"].start();
            let prover_input = generate_proof_input(
                PathBuf::from(bin_path),
                replay_record.block_context,
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
                root_hash: tree
                    .read()
                    .unwrap()
                    .storage_tree
                    .root()
                    .as_u8_array_ref()
                    .into(),
                leaf_count: tree.read().unwrap().storage_tree.next_free_slot,
            };

            let commit_batch_info = CommitBatchInfo::new(
                batch_output,
                replay_record.l1_transactions,
                tree_output,
                CHAIN_ID,
            );
            tracing::debug!("Expected commit batch info: {:?}", commit_batch_info);

            if let Some(commit_batch_info_sender) = &commit_batch_info_sender {
                rt.block_on(commit_batch_info_sender.send(commit_batch_info.clone()))
                    .unwrap();
            }

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

/// Receives `CommitBatchInfo`s from all workers, buffers anything that
/// arrives early, and forwards them strictly in block-number order.
///
/// todo: will be removed/replaced when batcher is reworked to use persistent tree
async fn ordered_committer(
    mut rx: Receiver<CommitBatchInfo>,
    l1: L1SenderHandle,
) -> anyhow::Result<()> {
    use std::collections::BTreeMap;

    // batcher always starts from the first block for now
    // since it needs to build the in-memory tree.
    let mut expected = 1;
    let mut buffer = BTreeMap::new(); // early arrivals keyed by block_no

    while let Some(info) = rx.recv().await {
        let bn = info.batch_number;
        buffer.insert(bn, info);

        while let Some(in_order) = buffer.remove(&expected) {
            l1.commit(in_order).await?; // <-- now it is definitely in order
            expected += 1;
        }
    }
    Ok(())
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
