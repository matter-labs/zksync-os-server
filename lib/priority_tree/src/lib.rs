use crate::db::PriorityTreeDB;
use alloy::primitives::{B256, BlockNumber, TxHash};
use anyhow::Context;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{Mutex, mpsc};
use zksync_os_contract_interface::models::PriorityOpsBatchInfo;
use zksync_os_crypto::hasher::Hasher;
use zksync_os_crypto::hasher::keccak::KeccakHasher;
use zksync_os_l1_sender::batcher_model::{BatchEnvelope, FriProof};
use zksync_os_l1_sender::commands::execute::ExecuteCommand;
use zksync_os_mini_merkle_tree::{HashEmptySubtree, MiniMerkleTree};
use zksync_os_observability::{ComponentStateReporter, GenericComponentState};
use zksync_os_storage_api::{ReadBatch, ReadReplay};
use zksync_os_types::ZkEnvelope;

mod db;

#[derive(Clone)]
pub struct PriorityTreeManager<ReplayStorage> {
    merkle_tree: Arc<Mutex<MiniMerkleTree<PriorityOpsLeaf>>>,
    replay_storage: ReplayStorage,
    db: PriorityTreeDB,
}

impl<ReplayStorage: ReadReplay> PriorityTreeManager<ReplayStorage> {
    pub fn new(
        replay_storage: ReplayStorage,
        last_executed_block: BlockNumber,
        db_path: &Path,
    ) -> anyhow::Result<Self> {
        let started_at = Instant::now();
        let db = PriorityTreeDB::new(db_path);
        let (initial_block_number, mut merkle_tree) = db.init_tree()?;

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
            last_executed_block = last_executed_block,
            root = ?merkle_tree.merkle_root(),
            time_taken = ?started_at.elapsed(),
            "re-built priority tree"
        );

        Ok(Self {
            merkle_tree: Arc::new(Mutex::new(merkle_tree)),
            replay_storage,
            db,
        })
    }

    /// Keeps building the tree by adding new transactions to the priority tree. As an input it can accept either
    /// `proved_batch_envelopes_receiver` or `proved_batch_numbers_receiver`. Only one of them must be provided.
    /// If `proved_batch_envelopes_receiver` is provided, then `execute_batches_sender` can also be provided
    /// to forward the proved batches along with the priority ops proofs.
    pub async fn prepare_execute_commands<BatchStorage: ReadBatch>(
        self,
        batch_storage: BatchStorage,
        mut proved_batch_envelopes_receiver: Option<mpsc::Receiver<BatchEnvelope<FriProof>>>,
        mut proved_batch_numbers_receiver: Option<mpsc::Receiver<u64>>,
        priority_ops_count_sender: mpsc::Sender<(u64, u64, usize)>,
        execute_batches_sender: Option<mpsc::Sender<ExecuteCommand>>,
    ) -> anyhow::Result<()> {
        assert!(
            proved_batch_envelopes_receiver.is_some() ^ proved_batch_numbers_receiver.is_some(),
            "exactly one of `proved_batch_envelopes_receiver` or `proved_batch_numbers_receiver` must be provided"
        );
        assert!(
            execute_batches_sender.is_none() || proved_batch_envelopes_receiver.is_some(),
            "`execute_batches_sender` can only be provided if `proved_batch_envelopes_receiver` is also provided"
        );

        let latency_tracker = ComponentStateReporter::global().handle_for(
            "priority_tree_manager#prepare_execute_commands",
            GenericComponentState::Processing,
        );

        async fn take_n<T>(receiver: &mut mpsc::Receiver<T>, n: usize) -> anyhow::Result<Vec<T>> {
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
            // todo(#160): we enforce executing one batch at a time for now as we don't have
            //             aggregation seal criteria yet
            let (batch_envelopes, batch_ranges) = match (
                &mut proved_batch_envelopes_receiver,
                &mut proved_batch_numbers_receiver,
            ) {
                (Some(r), _) => {
                    let envelopes = take_n(r, 1).await?;
                    let ranges = envelopes
                        .iter()
                        .map(|e| {
                            (
                                e.batch.commit_batch_info.batch_number,
                                (e.batch.first_block_number, e.batch.last_block_number),
                            )
                        })
                        .collect::<Vec<_>>();
                    (Some(envelopes), ranges)
                }
                (None, Some(r)) => {
                    let batch_numbers = take_n(r, 1).await?;
                    let mut ranges = Vec::new();
                    for i in batch_numbers {
                        let range = batch_storage
                            .get_batch_range_by_number(i)
                            .await
                            .with_context(|| format!("failed to get batch {i} from storage"))?
                            .with_context(|| format!("batch {i} not found in storage"))?;
                        ranges.push((i, range));
                    }
                    (None, ranges)
                }
                _ => unreachable!(),
            };
            latency_tracker.enter_state(GenericComponentState::Processing);
            let mut priority_ops = Vec::new();
            let mut merkle_tree = self.merkle_tree.lock().await;
            for (batch_number, (first_block_number, last_block_number)) in batch_ranges {
                let mut first_priority_op_id_in_batch = None;
                let mut priority_op_count = 0;
                for block_number in first_block_number..=last_block_number {
                    let replay = self.replay_storage.get_replay_record(block_number).unwrap();
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
                    batch_number, last_block_number, priority_op_count,
                    "Processing batch in priority tree manager"
                );

                latency_tracker.enter_state(GenericComponentState::WaitingSend);
                priority_ops_count_sender
                    .send((batch_number, last_block_number, priority_op_count))
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
        }
    }

    /// Keeps caching the priority tree after each batch execution.
    pub async fn keep_caching(
        self,
        mut executed_batch_numbers_receiver: mpsc::Receiver<u64>,
        mut priority_ops_count_receiver: mpsc::Receiver<(u64, u64, usize)>,
    ) -> anyhow::Result<()> {
        let latency_tracker = ComponentStateReporter::global().handle_for(
            "priority_tree_manager#keep_caching",
            GenericComponentState::Processing,
        );

        loop {
            latency_tracker.enter_state(GenericComponentState::WaitingRecv);
            let new_executed_batch_number = executed_batch_numbers_receiver
                .recv()
                .await
                .context("`executed_batch_numbers_receiver` closed")?;
            let (batch_number, last_block_number, priority_ops_count) = priority_ops_count_receiver
                .recv()
                .await
                .context("`priority_ops_count_receiver` closed")?;
            assert_eq!(
                batch_number, new_executed_batch_number,
                "Channels are out of sync"
            );

            latency_tracker.enter_state(GenericComponentState::Processing);
            let mut tree = self.merkle_tree.lock().await;
            if priority_ops_count > 0 {
                tree.trim_start(priority_ops_count);
            }
            self.db
                .cache_tree(&tree, last_block_number)
                .context("failed to cache tree")?;
            tracing::debug!(batch_number, "cached priority tree");
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
