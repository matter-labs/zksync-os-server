//! This module determines the fees to pay in txs containing blocks submitted to the L1.

use alloy::providers::{DynProvider, Provider};
use metrics::METRICS;
use std::time::Duration;
use std::{
    collections::VecDeque,
    sync::{Arc, RwLock},
};
use tokio::sync::watch;

mod metrics;
mod provider;
#[cfg(test)]
mod tests;

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
    pub l1_gas_pricing_multiplier: f64,
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
        this.pubdata_price_sender.send_replace(Some(this.pubdata_price_inner()));

        Ok(this)
    }

    /// Performs an actualization routine for `GasAdjuster`.
    /// This method is intended to be invoked periodically.
    pub async fn keep_updated(&self) -> anyhow::Result<()> {
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
                .send_replace(Some(self.pubdata_price_inner()));
        }
        Ok(())
    }

    pub async fn run(self: Arc<Self>) -> anyhow::Result<()> {
        let mut timer = tokio::time::interval(self.config.poll_period);
        let mut attempts_failed_in_a_row = 0usize;
        loop {
            if let Err(err) = self.keep_updated().await {
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

    fn gas_price_inner(&self) -> u128 {
        let median = self.base_fee_statistics.median();
        let effective_gas_price = median + self.config.max_priority_fee_per_gas;

        (self.config.l1_gas_pricing_multiplier * effective_gas_price as f64) as u128
    }

    fn pubdata_price_inner(&self) -> u128 {
        match self.config.pubdata_mode {
            PubdataMode::Blobs => {
                const BLOB_GAS_PER_BYTE: u128 = 1; // `BYTES_PER_BLOB` = `GAS_PER_BLOB` = 2 ^ 17.

                let blob_base_fee_median = self.blob_base_fee_statistics.median();
                let calculated_price = (blob_base_fee_median * BLOB_GAS_PER_BYTE) as f64
                    * self.config.pubdata_pricing_multiplier;

                calculated_price as u128
            }
            PubdataMode::Calldata => {
                /// The amount of gas we need to pay for each non-zero pubdata byte.
                /// Note that it is bigger than 16 to account for potential overhead.
                const L1_GAS_PER_PUBDATA_BYTE: u128 = 17;

                self.gas_price_inner()
                    .saturating_mul(L1_GAS_PER_PUBDATA_BYTE)
            }
            PubdataMode::Validium => 0,
        }
    }
}

/// Helper structure responsible for collecting the data about recent transactions,
/// calculating the median base fee.
#[derive(Debug, Clone, Default)]
pub(crate) struct GasStatisticsInner<T> {
    samples: VecDeque<T>,
    median_cached: T,
    max_samples: usize,
    last_processed_block: u64,
}

impl<T: Ord + Copy + Default> GasStatisticsInner<T> {
    fn new(max_samples: usize, block: u64, fee_history: impl IntoIterator<Item = T>) -> Self {
        let mut statistics = Self {
            max_samples,
            samples: VecDeque::with_capacity(max_samples),
            median_cached: T::default(),
            last_processed_block: 0,
        };

        statistics.add_samples(fee_history);

        Self {
            last_processed_block: block,
            ..statistics
        }
    }

    fn median(&self) -> T {
        self.median_cached
    }

    fn add_samples(&mut self, fees: impl IntoIterator<Item = T>) {
        let old_len = self.samples.len();
        self.samples.extend(fees);
        let processed_blocks = self.samples.len() - old_len;
        self.last_processed_block += processed_blocks as u64;

        let extra = self.samples.len().saturating_sub(self.max_samples);
        self.samples.drain(..extra);

        let mut samples: Vec<_> = self.samples.iter().cloned().collect();

        if !self.samples.is_empty() {
            let (_, &mut median, _) = samples.select_nth_unstable(self.samples.len() / 2);
            self.median_cached = median;
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct GasStatistics<T>(RwLock<GasStatisticsInner<T>>);

impl<T: Ord + Copy + Default> GasStatistics<T> {
    pub fn new(max_samples: usize, block: u64, fee_history: impl IntoIterator<Item = T>) -> Self {
        Self(RwLock::new(GasStatisticsInner::new(
            max_samples,
            block,
            fee_history,
        )))
    }

    pub fn median(&self) -> T {
        self.0.read().unwrap().median()
    }

    pub fn add_samples(&self, fees: impl IntoIterator<Item = T>) {
        self.0.write().unwrap().add_samples(fees)
    }

    pub fn last_processed_block(&self) -> u64 {
        self.0.read().unwrap().last_processed_block
    }
}
