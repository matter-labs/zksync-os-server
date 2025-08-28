use zk_ee::common_structs::MAX_NUMBER_OF_LOGS;
use zk_ee::system::MAX_NATIVE_COMPUTATIONAL;
use zk_os_forward_system::run::BlockOutput;
use zksync_os_l1_sender::batcher_metrics::BATCHER_METRICS;

#[derive(Default, Clone)]
pub(crate) struct BatchInfoAccumulator {
    // Accumulated values
    pub native_cycles: u64,
    pub pubdata_bytes: u64,
    pub l2_to_l1_logs_count: u64,

    // Limits
    pub blocks_per_batch_limit: usize,
    pub batch_pubdata_limit_bytes: u64,
}

impl BatchInfoAccumulator {
    pub fn new(blocks_per_batch_limit: usize, batch_pubdata_limit_bytes: u64) -> Self {
        Self {
            blocks_per_batch_limit,
            batch_pubdata_limit_bytes,
            ..Default::default()
        }
    }

    pub fn add(&mut self, block_output: &BlockOutput) -> &Self {
        self.native_cycles += block_output.computaional_native_used;
        self.pubdata_bytes += block_output.pubdata.len() as u64;
        self.l2_to_l1_logs_count += block_output
            .tx_results
            .iter()
            .map(|tx_result| tx_result.as_ref().map_or(0, |tx| tx.l2_to_l1_logs.len()))
            .sum::<usize>() as u64;

        self
    }

    /// Checks if the batch should be sealed based on the content of the blocks.
    /// e.g. due to the block count limit, tx count limit, or pubdata size limit.
    pub fn is_batch_limit_reached(&self, blocks_len: usize) -> bool {
        if blocks_len > self.blocks_per_batch_limit {
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
}
