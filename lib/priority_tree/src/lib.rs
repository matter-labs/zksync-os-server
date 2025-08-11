use alloy::primitives::{B256, BlockNumber, TxHash};
use anyhow::Context;
use std::time::Instant;
use tokio::sync::mpsc;
use zksync_os_contract_interface::models::PriorityOpsBatchInfo;
use zksync_os_crypto::hasher::Hasher;
use zksync_os_crypto::hasher::keccak::KeccakHasher;
use zksync_os_l1_sender::commands::execute::ExecuteCommand;
use zksync_os_l1_sender::model::{BatchEnvelope, FriProof};
use zksync_os_mini_merkle_tree::{HashEmptySubtree, MiniMerkleTree};
use zksync_os_storage_api::ReadReplay;
use zksync_os_types::ZkEnvelope;

pub struct PriorityTreeManager<ReplayStorage> {
    replay_storage: ReplayStorage,
    last_executed_block: BlockNumber,
    // == plumbing ==
    // inbound
    batch_l1_proved_receiver: mpsc::Receiver<BatchEnvelope<FriProof>>,

    // outbound
    execute_batches_sender: mpsc::Sender<ExecuteCommand>,
}

impl<ReplayStorage: ReadReplay> PriorityTreeManager<ReplayStorage> {
    pub fn new(
        replay_storage: ReplayStorage,
        last_executed_block: BlockNumber,
        // == plumbing ==
        // inbound
        batch_l1_proved_receiver: mpsc::Receiver<BatchEnvelope<FriProof>>,
        // outbound
        execute_batches_sender: mpsc::Sender<ExecuteCommand>,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            replay_storage,
            last_executed_block,
            batch_l1_proved_receiver,
            execute_batches_sender,
        })
    }

    pub async fn run(mut self) -> anyhow::Result<()> {
        let started_at = Instant::now();
        let mut l1_tx_hashes = Vec::new();
        tracing::debug!(
            last_executed_block = self.last_executed_block,
            "starting to re-build priority tree"
        );
        for block_number in 1..=self.last_executed_block {
            let block = self
                .replay_storage
                .get_replay_record(block_number)
                .with_context(|| {
                    format!("cannot re-build priority tree: missing replay block {block_number}")
                })?;
            for tx in block.transactions {
                match tx.into_envelope() {
                    ZkEnvelope::L1(l1_tx) => l1_tx_hashes.push(l1_tx.hash().0.into()),
                    ZkEnvelope::L2(_) => {}
                }
            }
        }
        let mut merkle_tree = MiniMerkleTree::<PriorityOpsLeaf>::from_hashes(
            KeccakHasher,
            l1_tx_hashes.into_iter(),
            None,
        );
        tracing::info!(
            last_executed_block = self.last_executed_block,
            root = ?merkle_tree.merkle_root(),
            time_taken = ?started_at.elapsed(),
            "re-built priority tree"
        );

        loop {
            // todo(#160): we enforce executing one batch at a time for now as we don't have
            //             aggregation seal criteria yet
            let batches = self.take_n(1).await?;
            let mut priority_ops = Vec::new();
            for batch in &batches {
                let count = batch.batch.commit_batch_info.number_of_layer1_txs as usize;
                if batch.batch.commit_batch_info.number_of_layer1_txs == 0 {
                    // Short-circuit for batches with no L1 txs
                    priority_ops.push(PriorityOpsBatchInfo::default());
                    continue;
                }
                let mut first_priority_op_id_in_batch = None;
                for block_number in batch.batch.first_block_number..=batch.batch.last_block_number {
                    let replay = self.replay_storage.get_replay_record(block_number).unwrap();
                    for tx in replay.transactions {
                        match tx.into_envelope() {
                            ZkEnvelope::L1(l1_tx) => {
                                first_priority_op_id_in_batch
                                    .get_or_insert(l1_tx.priority_id() as usize);
                                merkle_tree.push_hash(l1_tx.hash().0.into());
                            }
                            ZkEnvelope::L2(_) => {}
                        }
                    }
                }
                // We cache paths for priority transactions that happened in the previous batches.
                // For this we absorb all the elements up to `first_priority_op_id_in_batch`.`
                merkle_tree.trim_start(
                    first_priority_op_id_in_batch.expect("at least one L1 tx")
                        - merkle_tree.start_index(),
                );
                let (_, left, right) = merkle_tree.merkle_root_and_paths_for_range(..count);
                let hashes = merkle_tree.hashes_prefix(count);
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
            self.execute_batches_sender
                .send(ExecuteCommand::new(batches, priority_ops))
                .await?;
        }
    }

    pub async fn take_n(&mut self, n: usize) -> anyhow::Result<Vec<BatchEnvelope<FriProof>>> {
        let mut out = Vec::default();
        while out.len() < n {
            match self.batch_l1_proved_receiver.recv().await {
                Some(v) => out.push(v),
                None => anyhow::bail!("channel closed"),
            }
        }
        Ok(out)
    }
}

// Custom dummy type that forces empty leaf hashes to contain keccak256([]) inside.
struct PriorityOpsLeaf;

impl HashEmptySubtree<PriorityOpsLeaf> for KeccakHasher {
    fn empty_leaf_hash(&self) -> B256 {
        self.hash_bytes(&[])
    }
}
