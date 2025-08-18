use zk_ee::common_structs::MAX_NUMBER_OF_LOGS;
use zk_ee::system::MAX_NATIVE_COMPUTATIONAL;
use zk_os_forward_system::run::BlockOutput;
use crate::config::BatcherConfig;

#[derive(Default, Clone)]
pub(crate) struct BatchInfoAccumulator {
    pub tx_count: u64,
    pub gas_used: u64,
    pub native_cycles: u64,
    pub pubdata_bytes: u64,
    pub l2_to_l1_logs_count: u64,
}

impl BatchInfoAccumulator {
    pub fn add(&mut self, block_output: &BlockOutput) -> &Self {
        self.tx_count += block_output.tx_results.len() as u64;
        self.gas_used += block_output.header.gas_used;
        // self.native_cycles += block_output.native_cycles;
        self.pubdata_bytes += block_output.pubdata.len() as u64;
        self.l2_to_l1_logs_count += block_output.tx_results.iter().map(|tx_result| { tx_result.as_ref().map_or(0, |tx| tx.l2_to_l1_logs.len()) }).sum::<usize>() as u64;

        self
    }

    /// Checks if the batch should be sealed based on the content of the blocks.
    /// e.g. due to the block count limit, tx count limit, or pubdata size limit.
    pub fn is_batch_limit_reached(
        &self,
        batcher_config: &BatcherConfig,
        blocks_len: usize,
    ) -> bool {
        if blocks_len > batcher_config.blocks_per_batch_limit {
            tracing::debug!("Batcher: reached blocks per batch limit");
            return true;
        }

        if self.tx_count >= batcher_config.transactions_per_batch_limit {
            tracing::debug!("Batcher: reached transactions per batch limit");
            return true;
        }

        if self.gas_used >= batcher_config.batch_gas_limit {
            tracing::debug!("Batcher: reached gas limit for the batch");
            return true;
        }

        if self.native_cycles >= MAX_NATIVE_COMPUTATIONAL {
            tracing::debug!("Batcher: reached native cycles limit for the batch");
            return true;
        }

        if self.pubdata_bytes >= batcher_config.batch_pubdata_limit_bytes {
            tracing::debug!("Batcher: reached pubdata bytes limit for the batch");
            return true;
        }

        if self.l2_to_l1_logs_count >= MAX_NUMBER_OF_LOGS {
            tracing::debug!("Batcher: reached max number of L2 to L1 logs");
            return true;
        }

        false
    }
}
