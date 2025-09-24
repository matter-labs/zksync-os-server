use crate::db::PriorityTreeDB;
use alloy::primitives::{B256, TxHash};
use anyhow::Context;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, mpsc};
use zksync_os_contract_interface::models::PriorityOpsBatchInfo;
use zksync_os_crypto::hasher::Hasher;
use zksync_os_crypto::hasher::keccak::KeccakHasher;
use zksync_os_l1_sender::batcher_model::{FriProof, SignedBatchEnvelope};
use zksync_os_l1_sender::commands::execute::ExecuteCommand;
use zksync_os_mini_merkle_tree::{HashEmptySubtree, MiniMerkleTree};
use zksync_os_observability::{ComponentStateReporter, GenericComponentState};
use zksync_os_pipeline::PeekableReceiver;
use zksync_os_storage_api::{ReadBatch, ReadFinality, ReadReplay, ReplayRecord};
use zksync_os_types::ZkEnvelope;

mod db;

#[derive(Clone)]
pub struct PriorityTreeManager<ReplayStorage, Finality, BatchStorage> {
    merkle_tree: Arc<Mutex<MiniMerkleTree<PriorityOpsLeaf>>>,
    replay_storage: ReplayStorage,
    db: PriorityTreeDB,
    finality: Finality,
    batch_storage: BatchStorage,
    last_executed_batch_on_init: u64,
}

impl<ReplayStorage: ReadReplay, Finality: ReadFinality, BatchStorage: ReadBatch>
    PriorityTreeManager<ReplayStorage, Finality, BatchStorage>
{
    pub async fn new(
        replay_storage: ReplayStorage,
        db_path: &Path,
        finality: Finality,
        batch_storage: BatchStorage,
        last_executed_batch: u64,
    ) -> anyhow::Result<Self> {
        let started_at = Instant::now();
        let db = PriorityTreeDB::new(db_path);
        let (initial_block_number, mut merkle_tree) = db.init_tree()?;
        let last_executed_block = batch_storage
            .get_batch_range_by_number(last_executed_batch)
            .await
            .with_context(|| {
                format!(
                    "cannot initialize priority tree: failed to get batch {last_executed_batch} from storage"
                )
            })?
            .with_context(|| {
                format!(
                    "cannot initialize priority tree: no batch {last_executed_batch} in storage"
                )
            })?
            .1;

        tracing::debug!(
            persisted_up_to = initial_block_number,
            last_executed_block = last_executed_block,
            "adding missing blocks to priority tree"
        );

        for block_number in (initial_block_number + 1)..=last_executed_block {
            let block = replay_storage
                .get_replay_record(block_number)
                .with_context(|| {
                    format!("cannot re-build priority tree: missing replay block {block_number}")
                })?;
            for tx in block.transactions {
                match tx.into_envelope() {
                    ZkEnvelope::L1(l1_tx) => merkle_tree.push_hash(*l1_tx.hash()),
                    ZkEnvelope::L2(_) => {}
                    ZkEnvelope::Upgrade(_) => {}
                }
            }
        }

        tracing::info!(
            last_executed_block,
            root = ?merkle_tree.merkle_root(),
            time_taken = ?started_at.elapsed(),
            "re-built priority tree"
        );

        Ok(Self {
            merkle_tree: Arc::new(Mutex::new(merkle_tree)),
            replay_storage,
            db,
            finality,
            batch_storage,
            last_executed_batch_on_init: last_executed_batch,
        })
    }

    /// Keeps building the tree by adding new transactions to the priority tree.
    /// It supports two modes of operation:
    /// - For the main node: you must provide both `proved_batch_envelopes_receiver` and `execute_batches_sender`
    ///   and it will forward the proven batch envelopes along with the priority ops proofs.
    /// - For the EN: you must provide neither `proved_batch_envelopes_receiver` nor `execute_batches_sender`
    ///   and it will keep adding new transactions to the tree for finalized blocks.
    pub async fn prepare_execute_commands(
        self,
        main_node_channels: Option<(
            PeekableReceiver<SignedBatchEnvelope<FriProof>>,
            mpsc::Sender<ExecuteCommand>,
        )>,
        priority_ops_internal_sender: mpsc::Sender<(u64, u64, Option<usize>)>,
    ) -> anyhow::Result<()> {
        let latency_tracker = ComponentStateReporter::global().handle_for(
            "priority_tree_manager#prepare_execute_commands",
            GenericComponentState::Processing,
        );
        let (mut proved_batch_envelopes_receiver, execute_batches_sender) =
            main_node_channels.unzip();
        let mut last_processed_batch = self.last_executed_batch_on_init;

        async fn take_n<T>(receiver: &mut PeekableReceiver<T>, n: usize) -> anyhow::Result<Vec<T>> {
            let mut out = Vec::default();
            while out.len() < n {
                match receiver.recv().await {
                    Some(v) => out.push(v),
                    None => anyhow::bail!("channel closed"),
                }
            }
            Ok(out)
        }

        loop {
            latency_tracker.enter_state(GenericComponentState::WaitingRecv);
            let (batch_envelopes, batch_ranges) = match proved_batch_envelopes_receiver.as_mut() {
                Some(r) => {
                    // todo(#160): we enforce executing one batch at a time for now as we don't have
                    //             aggregation seal criteria yet
                    let envelopes = take_n(r, 1).await?;
                    let ranges = envelopes
                        .iter()
                        .map(|e| {
                            (
                                e.batch.batch_info.batch_number,
                                (e.batch.first_block_number, e.batch.last_block_number),
                            )
                        })
                        .collect::<Vec<_>>();
                    // Sanity check: we must receive the next batch in sequence.
                    assert_eq!(
                        ranges[0].0,
                        last_processed_batch + 1,
                        "Unexpected envelope received"
                    );
                    (Some(envelopes), ranges)
                }
                None => {
                    let _ = self
                        .finality
                        .subscribe()
                        .wait_for(|f| last_processed_batch < f.last_executed_batch)
                        .await
                        .context("failed to wait for next finalized batch")?;

                    let next_batch_number = last_processed_batch + 1;
                    let range = self
                        .batch_storage
                        .get_batch_range_by_number(next_batch_number)
                        .await
                        .with_context(|| {
                            format!("failed to get batch {next_batch_number} from storage")
                        })?
                        .with_context(|| {
                            format!("batch {next_batch_number} not found in storage")
                        })?;
                    let ranges = vec![(next_batch_number, range)];
                    (None, ranges)
                }
            };
            latency_tracker.enter_state(GenericComponentState::Processing);
            let mut priority_ops = Vec::new();
            let mut merkle_tree = self.merkle_tree.lock().await;
            for (batch_number, (first_block_number, last_block_number)) in batch_ranges.clone() {
                let mut first_priority_op_id_in_batch = None;
                let mut priority_op_count = 0;
                for block_number in first_block_number..=last_block_number {
                    // Block is not guaranteed to be present in the replay storage for EN, so we use `wait_for_replay_record`.
                    let replay = self.wait_for_replay_record(block_number).await;
                    for tx in replay.transactions {
                        match tx.into_envelope() {
                            ZkEnvelope::L1(l1_tx) => {
                                first_priority_op_id_in_batch
                                    .get_or_insert(l1_tx.priority_id() as usize);
                                priority_op_count += 1;
                                merkle_tree.push_hash(l1_tx.hash().0.into());
                            }
                            ZkEnvelope::L2(_) => {}
                            ZkEnvelope::Upgrade(_) => {}
                        }
                    }
                }
                tracing::debug!(
                    batch_number,
                    last_block_number,
                    priority_op_count,
                    "Processing batch in priority tree manager"
                );

                latency_tracker.enter_state(GenericComponentState::WaitingSend);
                priority_ops_internal_sender
                    .send((
                        batch_number,
                        last_block_number,
                        first_priority_op_id_in_batch.map(|id| id + priority_op_count - 1),
                    ))
                    .await
                    .context("failed to send priority ops count")?;
                latency_tracker.enter_state(GenericComponentState::Processing);

                if first_priority_op_id_in_batch.is_none() {
                    // Short-circuit for batches with no L1 txs.
                    priority_ops.push(PriorityOpsBatchInfo::default());
                    continue;
                }
                let range = {
                    let start = first_priority_op_id_in_batch.expect("at least one L1 tx")
                        - merkle_tree.start_index();
                    start..(start + priority_op_count)
                };
                tracing::trace!(
                    "getting merkle paths for priority ops range {range:?}, merkle_tree.start_index() = {}, merkle_tree.length() = {}",
                    merkle_tree.start_index(),
                    merkle_tree.length(),
                );

                let (_, left, right) = merkle_tree.merkle_root_and_paths_for_range(range.clone());
                let hashes = merkle_tree.hashes_range(range);
                priority_ops.push(PriorityOpsBatchInfo {
                    left_path: left
                        .into_iter()
                        .map(Option::unwrap_or_default)
                        .map(|hash| TxHash::from(hash.0))
                        .collect(),
                    right_path: right
                        .into_iter()
                        .map(Option::unwrap_or_default)
                        .map(|hash| TxHash::from(hash.0))
                        .collect(),
                    item_hashes: hashes
                        .into_iter()
                        .map(|hash| TxHash::from(hash.0))
                        .collect(),
                });
            }
            drop(merkle_tree);
            if let Some(s) = &execute_batches_sender {
                latency_tracker.enter_state(GenericComponentState::WaitingSend);
                s.send(ExecuteCommand::new(batch_envelopes.unwrap(), priority_ops))
                    .await?;
            }
            last_processed_batch = batch_ranges.last().unwrap().0;
        }
    }

    /// Keeps caching the priority tree after each batch execution.
    pub async fn keep_caching(
        self,
        mut priority_ops_internal_receiver: mpsc::Receiver<(u64, u64, Option<usize>)>,
    ) -> anyhow::Result<()> {
        let latency_tracker = ComponentStateReporter::global().handle_for(
            "priority_tree_manager#keep_caching",
            GenericComponentState::Processing,
        );
        let mut finality_receiver = self.finality.subscribe();

        loop {
            latency_tracker.enter_state(GenericComponentState::WaitingRecv);
            let (batch_number, last_block_number, last_priority_op_id) =
                priority_ops_internal_receiver
                    .recv()
                    .await
                    .context("`priority_ops_internal_receiver` closed")?;
            finality_receiver
                .wait_for(|f| last_block_number <= f.last_executed_block)
                .await
                .context("failed to wait for executed block number")?;

            latency_tracker.enter_state(GenericComponentState::Processing);
            let mut tree = self.merkle_tree.lock().await;
            if let Some(last_priority_op_id) = last_priority_op_id {
                let leaves_to_trim = (last_priority_op_id + 1)
                    .checked_sub(tree.start_index())
                    .unwrap();
                tree.trim_start(leaves_to_trim);
                self.db
                    .cache_tree(&tree, last_block_number)
                    .context("failed to cache tree")?;
                tracing::debug!(batch_number, "cached priority tree");
            }
        }
    }

    async fn wait_for_replay_record(&self, block_number: u64) -> ReplayRecord {
        let mut timer = tokio::time::interval(Duration::from_millis(100));
        loop {
            timer.tick().await;
            if let Some(r) = self.replay_storage.get_replay_record(block_number) {
                return r;
            }
        }
    }
}

// Custom dummy type that forces empty leaf hashes to contain keccak256([]) inside.
struct PriorityOpsLeaf;

impl HashEmptySubtree<PriorityOpsLeaf> for KeccakHasher {
    fn empty_leaf_hash(&self) -> B256 {
        self.hash_bytes(&[])
    }
}
