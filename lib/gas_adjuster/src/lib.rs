//! This module determines the fees to pay in txs containing blocks submitted to the L1.

use crate::statistics::GasStatistics;
use alloy::providers::{DynProvider, Provider};
use metrics::METRICS;
use std::time::Duration;
use tokio::sync::watch;

mod metrics;
mod statistics;

/// This component keeps track of the median `base_fee` from the last `max_base_fee_samples` blocks.
///
/// It also tracks the median `blob_base_fee` from the last `max_blob_base_fee_sample` blocks.
/// It is used to adjust the base_fee of transactions sent to L1.
#[derive(Debug)]
pub struct GasAdjuster {
    base_fee_statistics: GasStatistics<u128>,
    blob_base_fee_statistics: GasStatistics<u128>,

    config: GasAdjusterConfig,
    provider: DynProvider,
    pubdata_price_sender: watch::Sender<Option<u128>>,
}

#[derive(Debug, Clone)]
pub enum PubdataMode {
    Blobs,
    Calldata,
    Validium,
}

#[derive(Debug)]
pub struct GasAdjusterConfig {
    pub pubdata_mode: PubdataMode,
    pub max_base_fee_samples: usize,
    pub num_samples_for_blob_base_fee_estimate: usize,
    pub max_priority_fee_per_gas: u128,
    pub poll_period: Duration,
    pub pubdata_pricing_multiplier: f64,
}

impl GasAdjuster {
    pub async fn new(
        provider: DynProvider,
        config: GasAdjusterConfig,
        pubdata_price_sender: watch::Sender<Option<u128>>,
    ) -> anyhow::Result<Self> {
        // Subtracting 1 from the "latest" block number to prevent errors in case
        // the info about the latest block is not yet present on the node.
        // This sometimes happens on Infura.
        let current_block = provider.get_block_number().await?.saturating_sub(1);
        let fee_history =
            Self::base_fee_history(&provider, current_block, config.max_base_fee_samples as u64)
                .await?;

        let base_fee_statistics = GasStatistics::new(
            config.max_base_fee_samples,
            current_block,
            fee_history.iter().map(|fee| fee.base_fee_per_gas),
        );

        let blob_base_fee_statistics = GasStatistics::new(
            config.num_samples_for_blob_base_fee_estimate,
            current_block,
            fee_history.iter().map(|fee| fee.base_fee_per_blob_gas),
        );

        let this = Self {
            base_fee_statistics,
            blob_base_fee_statistics,
            config,
            provider,
            pubdata_price_sender,
        };
        this.pubdata_price_sender
            .send_replace(Some(this.pubdata_price()));

        Ok(this)
    }

    /// Performs an actualization routine for `GasAdjuster`.
    /// This method is intended to be invoked periodically.
    pub async fn update_fees(&mut self) -> anyhow::Result<()> {
        // Subtracting 1 from the "latest" block number to prevent errors in case
        // the info about the latest block is not yet present on the node.
        // This sometimes happens on Infura.
        let current_block = self.provider.get_block_number().await?.saturating_sub(1);

        let last_processed_block = self.base_fee_statistics.last_processed_block();

        if current_block > last_processed_block {
            let n_blocks = current_block - last_processed_block;
            let fee_data = Self::base_fee_history(&self.provider, current_block, n_blocks).await?;

            // We shouldn't rely on L1 provider to return consistent results, so we check that we have at least one new sample.
            if let Some(current_base_fee_per_gas) = fee_data.last().map(|fee| fee.base_fee_per_gas)
            {
                if current_base_fee_per_gas > u64::MAX as u128 {
                    tracing::info!(
                        "Failed to report current_base_fee_per_gas = {current_base_fee_per_gas}, it exceeds u64::MAX"
                    );
                } else {
                    METRICS
                        .current_base_fee_per_gas
                        .set(current_base_fee_per_gas as u64);
                }
            }
            self.base_fee_statistics
                .add_samples(fee_data.iter().map(|fee| fee.base_fee_per_gas));
            if self.base_fee_statistics.median() <= u64::MAX as u128 {
                METRICS
                    .median_base_fee_per_gas
                    .set(self.base_fee_statistics.median() as u64);
            }

            if let Some(current_blob_base_fee) =
                fee_data.last().map(|fee| fee.base_fee_per_blob_gas)
            {
                if current_blob_base_fee > u64::MAX as u128 {
                    tracing::info!(
                        "Failed to report current_blob_base_fee = {current_blob_base_fee}, it exceeds u64::MAX"
                    );
                } else {
                    METRICS
                        .current_blob_base_fee
                        .set(current_blob_base_fee as u64);
                }
            }
            self.blob_base_fee_statistics
                .add_samples(fee_data.iter().map(|fee| fee.base_fee_per_blob_gas));
            if self.blob_base_fee_statistics.median() <= u64::MAX as u128 {
                METRICS
                    .median_blob_base_fee
                    .set(self.blob_base_fee_statistics.median() as u64);
            }

            self.pubdata_price_sender
                .send_replace(Some(self.pubdata_price()));
        }
        Ok(())
    }

    pub async fn run(mut self) -> anyhow::Result<()> {
        let mut timer = tokio::time::interval(self.config.poll_period);
        let mut attempts_failed_in_a_row = 0usize;
        loop {
            if let Err(err) = self.update_fees().await {
                attempts_failed_in_a_row += 1;
                if attempts_failed_in_a_row >= 5 {
                    tracing::warn!(
                        attempts_failed_in_a_row,
                        "Cannot add the base fee to gas statistics: {err}"
                    );
                }
            } else {
                attempts_failed_in_a_row = 0;
            }
            timer.tick().await;
        }
    }

    pub fn gas_price(&self) -> u128 {
        let median = self.base_fee_statistics.median();
        median + self.config.max_priority_fee_per_gas
    }

    pub fn pubdata_price(&self) -> u128 {
        let price = match self.config.pubdata_mode {
            PubdataMode::Blobs => {
                const BLOB_GAS_PER_BYTE: u128 = 1; // `BYTES_PER_BLOB` = `GAS_PER_BLOB` = 2 ^ 17.

                let blob_base_fee_median = self.blob_base_fee_statistics.median();
                blob_base_fee_median * BLOB_GAS_PER_BYTE
            }
            PubdataMode::Calldata => {
                /// The amount of gas we need to pay for each non-zero pubdata byte.
                /// Note that it is bigger than 16 to account for potential overhead.
                const L1_GAS_PER_PUBDATA_BYTE: u128 = 17;

                self.gas_price().saturating_mul(L1_GAS_PER_PUBDATA_BYTE)
            }
            PubdataMode::Validium => 0,
        };

        (self.config.pubdata_pricing_multiplier * price as f64) as u128
    }

    /// Collects the base fee history for the specified block range.
    ///
    /// Returns 1 value for each block in range, assuming that these blocks exist.
    /// Will return an error if the `upto_block` is beyond the head block.
    async fn base_fee_history(
        provider: &DynProvider,
        upto_block: u64,
        block_count: u64,
    ) -> anyhow::Result<Vec<BaseFees>> {
        const FEE_HISTORY_MAX_REQUEST_CHUNK: usize = 1023;

        let mut history = Vec::with_capacity(block_count as usize);
        let from_block = upto_block.saturating_sub(block_count - 1);

        // Here we are requesting `fee_history` from blocks
        // `[from_block; upto_block]` in chunks of size `FEE_HISTORY_MAX_REQUEST_CHUNK`
        // starting from the oldest block.
        for chunk_start in (from_block..=upto_block).step_by(FEE_HISTORY_MAX_REQUEST_CHUNK) {
            let chunk_end = (chunk_start + FEE_HISTORY_MAX_REQUEST_CHUNK as u64).min(upto_block);
            let chunk_size = chunk_end - chunk_start + 1;

            let fee_history = provider
                .get_fee_history(chunk_size, chunk_end.into(), &[])
                .await?;

            if fee_history.oldest_block != chunk_start {
                anyhow::bail!(
                    "unexpected `oldest_block`, expected: {chunk_start}, got {}",
                    fee_history.oldest_block
                );
            }

            // We take `chunk_size` entries and drop data for the block after `chunk_end`.
            for (base_fee_per_gas, base_fee_per_blob_gas) in fee_history
                .base_fee_per_gas
                .into_iter()
                .zip(fee_history.base_fee_per_blob_gas)
                .take(chunk_size as usize)
            {
                let fees = BaseFees {
                    base_fee_per_gas,
                    base_fee_per_blob_gas,
                };
                history.push(fees)
            }
        }

        Ok(history)
    }
}

/// Information about the base fees provided by the L1 client.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BaseFees {
    pub base_fee_per_gas: u128,
    pub base_fee_per_blob_gas: u128,
}
