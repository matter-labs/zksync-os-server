use alloy::primitives::B256;
use zk_ee::common_structs::MAX_NUMBER_OF_LOGS;
use zk_ee::system::MAX_NATIVE_COMPUTATIONAL;
use zk_os_forward_system::run::BlockOutput;
use zksync_os_l1_sender::batcher_metrics::BATCHER_METRICS;
use zksync_os_contract_interface::models::PriorityOpsBatchInfo;
use zksync_os_l1_sender::batcher_model::{BatchEnvelope, BatchMetadata, FriProof};
use zksync_os_l1_sender::commands::execute::ExecuteCommand;
use zksync_os_l1_sender::commands::L1SenderCommand;
use zksync_os_storage_api::ReplayRecord;
use zksync_os_types::ZkEnvelope;
use alloy::sol_types::SolCall;

#[derive(Default, Clone)]
pub(crate) struct BatchInfoAccumulator {
    // Accumulated values
    pub native_cycles: u64,
    pub pubdata_bytes: u64,
    pub l2_to_l1_logs_count: u64,
    pub block_count: u64,
    pub interop_roots_count: usize,
    pub l1_priority_txs_count: usize,
    pub last_priority_op_index: Option<u64>,

    // Limits
    pub blocks_per_batch_limit: u64,
    pub batch_pubdata_limit_bytes: u64,
}

impl BatchInfoAccumulator {
    pub fn new(blocks_per_batch_limit: u64, batch_pubdata_limit_bytes: u64) -> Self {
        Self {
            blocks_per_batch_limit,
            batch_pubdata_limit_bytes,
            ..Default::default()
        }
    }

    pub fn add(&mut self, block_output: &BlockOutput, replay_record: &ReplayRecord) -> &Self {
        self.native_cycles += block_output.computaional_native_used;
        self.pubdata_bytes += block_output.pubdata.len() as u64;
        self.l2_to_l1_logs_count += block_output
            .tx_results
            .iter()
            .map(|tx_result| tx_result.as_ref().map_or(0, |tx| tx.l2_to_l1_logs.len()))
            .sum::<usize>() as u64;
        self.block_count += 1;
        self.interop_roots_count += replay_record.block_context.interop_roots.roots().len();
        for tx in &replay_record.transactions {
            if let ZkEnvelope::L1(e) = tx.envelope() {
                self.l1_priority_txs_count += 1;
                self.last_priority_op_index = Some(e.priority_id());
            }
        }

        self
    }

    /// Checks if the batch should be sealed based on the content of the blocks.
    /// e.g. due to the block count limit, tx count limit, or pubdata size limit.
    pub fn is_batch_limit_reached(&self) -> bool {
        if self.block_count > self.blocks_per_batch_limit {
            BATCHER_METRICS.seal_reason[&"blocks_per_batch"].inc();
            tracing::debug!("Batcher: reached blocks per batch limit");
            return true;
        }

        if self.native_cycles > MAX_NATIVE_COMPUTATIONAL {
            BATCHER_METRICS.seal_reason[&"native_cycles"].inc();
            tracing::debug!("Batcher: reached native cycles limit for the batch");
            return true;
        }

        if self.pubdata_bytes > self.batch_pubdata_limit_bytes {
            BATCHER_METRICS.seal_reason[&"pubdata"].inc();
            tracing::debug!("Batcher: reached pubdata bytes limit for the batch");
            return true;
        }

        if self.l2_to_l1_logs_count > MAX_NUMBER_OF_LOGS {
            BATCHER_METRICS.seal_reason[&"l2_l1_logs"].inc();
            tracing::debug!("Batcher: reached max number of L2 to L1 logs");
            return true;
        }

        let execute_batches_calldata_size = Self::batch_execute_calldata_size(
            self.l1_priority_txs_count,
            self.last_priority_op_index,
            self.interop_roots_count,
        );

        const MAX_CALLDATA_SIZE: usize = 120_000;
        if execute_batches_calldata_size > MAX_CALLDATA_SIZE {
            BATCHER_METRICS.seal_reason[&"execute_calldata_size"].inc();
            tracing::debug!("Batcher: reached max calldata size for execute batch command");
            return true;
        }

        false
    }

    pub fn report_accumulated_resources_to_metrics(&self) {
        BATCHER_METRICS
            .computational_native_used_per_batch
            .observe(self.native_cycles);
        BATCHER_METRICS
            .pubdata_per_batch
            .observe(self.pubdata_bytes);
    }

    fn batch_execute_calldata_size(l1_priority_txs_count: usize, last_priority_op_index: Option<u64>, interop_roots_count: usize) -> usize {
        let mock_batch_envelope = BatchEnvelope::new(
            BatchMetadata::default(),
            FriProof::Fake,
        );
        let priority_ops_proof = if l1_priority_txs_count == 0 {
            PriorityOpsBatchInfo::default()
        } else {
            let merkle_tree_proof_len = (last_priority_op_index.unwrap() + 1).next_power_of_two().trailing_zeros() as usize;
            PriorityOpsBatchInfo {
                left_path: vec![B256::ZERO; merkle_tree_proof_len],
                right_path: vec![B256::ZERO; merkle_tree_proof_len],
                item_hashes: vec![B256::ZERO; l1_priority_txs_count],
            }
        };
        let interop_roots = vec![Default::default(); interop_roots_count];
        let command = ExecuteCommand::new(vec![mock_batch_envelope], vec![priority_ops_proof], vec![interop_roots]);
        command.solidity_call().abi_encoded_size()
    }
}
