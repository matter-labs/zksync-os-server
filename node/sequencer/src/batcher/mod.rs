use crate::commitment::{CommitBatchInfo, StoredBatchInfo};
use crate::conversions::{bytes32_to_h256, tx_abi_encode};
use crate::model::ReplayRecord;
use std::alloc::Global;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::mpsc::{Receiver, UnboundedSender};
use vise::{Buckets, Gauge, Histogram, LabeledFamily, Metrics, Unit};
use zk_os_basic_system::system_implementation::flat_storage_model::TestingTree;
use zk_os_forward_system::run::test_impl::{InMemoryTree, TxListSource};
use zk_os_forward_system::run::{generate_proof_input, BatchOutput, StorageCommitment};
use zksync_os_merkle_tree::{MerkleTreeReader, RocksDBWrapper};
use zksync_os_state::StateHandle;

const MAX_INFLIGHT: usize = 30;

/// This component will genearte l1 batches from the stream of blocks
/// It will also generate Prover Input for each batch.
///
/// Currently, batching is not implemented on zksync-os side, so we do 1 batch == 1 block
///
pub struct Batcher {
    block_receiver: tokio::sync::mpsc::Receiver<(BatchOutput, ReplayRecord)>,
    state_handle: StateHandle,
    bin_path: &'static str,
    num_workers: usize,
}

impl Batcher {
    pub fn new(
        // In the future it will only need ReplayRecord - it only uses BatchOutput for in-memory tree
        block_receiver: tokio::sync::mpsc::Receiver<(BatchOutput, ReplayRecord)>,

        state_handle: StateHandle,

        // todo: this is not used currently as we don't have impl `ReadStorageTree` for it
        _tree: MerkleTreeReader<RocksDBWrapper>,

        enable_logging: bool,
        num_workers: usize,
    ) -> Self {
        let bin_path = if enable_logging {
            "app_logging_enabled.bin"
        } else {
            "app.bin"
        };
        Self {
            block_receiver,
            state_handle,
            bin_path,
            num_workers,
        }
    }

    pub async fn run_loop(mut self) -> anyhow::Result<()> {
        // let (blocks_sender, _blocks_receiver) =
        //     tokio::sync::broadcast::channel::<(BatchOutput, ReplayRecord)>(1024);
        let mut worker_senders = Vec::new();

        let (proof_sender, mut proof_receiver) =
            tokio::sync::mpsc::unbounded_channel::<(u64, Vec<u32>)>();
        for worker_id in 0..self.num_workers {
            let (tx, rx) = tokio::sync::mpsc::channel(MAX_INFLIGHT);
            worker_senders.push(tx);
            let proof_sender = proof_sender.clone();
            let state_handle = self.state_handle.clone();
            std::thread::spawn(move || {
                worker_loop(
                    worker_id,
                    self.num_workers,
                    rx,
                    proof_sender,
                    state_handle,
                    self.bin_path,
                )
            });
        }

        let sending_task = async {
            loop {
                // Wait for a replay record from the channel
                match self.block_receiver.recv().await {
                    Some((batch_output, replay_record)) => {
                        BATCHER_METRICS
                            .current_block_number
                            .set(replay_record.context.block_number);
                        let block_number = replay_record.context.block_number;
                        tracing::info!(
                            "Batcher dispatcher received block {} with {} transactions",
                            block_number,
                            replay_record.transactions.len()
                        );
                        for tx in worker_senders.iter() {
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
        };

        let receiving_task = async {
            loop {
                // Wait for a replay record from the channel
                match proof_receiver.recv().await {
                    Some((block_number, proof)) => {
                        tracing::info!(
                            block_number = block_number,
                            "Batcher dispatcher received proof with length {}",
                            proof.len()
                        );
                    }
                    None => {
                        tracing::info!("Proof receiver closed, exiting batcher",);
                        break;
                    }
                }
            }
        };

        tokio::select! {
            _ = sending_task => {}
            _ = receiving_task => {}
        }

        Ok(())
    }
}

fn worker_loop(
    id: usize,
    num_workers: usize,
    mut rx: Receiver<(BatchOutput, ReplayRecord)>,
    // (block, prover input)
    proof_tx: UnboundedSender<(u64, Vec<u32>)>,
    state_handle: StateHandle,
    bin_path: &'static str,
) {
    tracing::info!("Worker {} started", id);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let tree = Arc::new(RwLock::new(InMemoryTree::<false> {
        storage_tree: TestingTree::new_in(Global),
        cold_storage: HashMap::new(),
    }));

    loop {
        let Some((batch_output, replay_record)) = rt.block_on(rx.recv()) else {
            tracing::warn!("Worker {} got None", id);
            break;
        };

        let bn = replay_record.context.block_number;
        tracing::info!("Worker {} got block {}", id, bn);

        /* 3. Run heavy job only when it’s my turn */
        if bn % num_workers as u64 == id as u64 {
            tracing::info!(
                worker = id,
                block_number = bn,
                "starting computing prover input",
            );

            let storage_commitment = StorageCommitment {
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
            let proof = generate_proof_input(
                PathBuf::from(bin_path),
                replay_record.context,
                storage_commitment,
                tree.clone(),
                state_view,
                list_source,
            )
            .expect("proof gen failed");

            let latency = prover_input_generation_latency.observe();

            tracing::info!(
                worker = id,
                block_number = bn,
                next_free_slot = tree.read().unwrap().storage_tree.next_free_slot,
                "Completed prover input in {:?}",
                latency
            );

            //
            // let u8s_prover_input: Vec<u8> = prover_input
            //     .into_iter()
            //     .flat_map(|x| x.to_le_bytes())
            //     .collect();
            //
            // let base64 = base64::encode(&u8s_prover_input);
            // fs::write(format!("prover_input_{}.txt", block_number), base64)?;

            proof_tx.send((bn, proof)).unwrap();
        }

        let mut write_tree = tree.write().unwrap();
        for w in &batch_output.storage_writes {
            write_tree.cold_storage.insert(w.key, w.value);
            write_tree.storage_tree.insert(&w.key, &w.value);
        }
        drop(write_tree);

        if bn % num_workers as u64 == id as u64 {
            let tree_output = zksync_os_merkle_tree::BatchOutput {
                root_hash: bytes32_to_h256(*tree.read().unwrap().storage_tree.root()),
                leaf_count: tree.read().unwrap().storage_tree.next_free_slot,
            };
            let commit_batch_info =
                CommitBatchInfo::new(batch_output, replay_record.transactions, tree_output);
            tracing::debug!("Expected commit batch info: {:?}", commit_batch_info);

            let stored_batch_info = StoredBatchInfo::from(commit_batch_info);
            tracing::debug!("Expected stored batch info: {:?}", stored_batch_info);

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
