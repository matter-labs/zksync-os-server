use crate::metrics::L1_STATE_METRICS;
use crate::models::BatchDaInputMode;
use crate::{Bridgehub, PubdataPricingMode, ZkChain};
use alloy::eips::BlockId;
use alloy::primitives::{Address, U256};
use alloy::providers::{DynProvider, Provider};
use anyhow::Context;
use backon::{ConstantBuilder, Retryable};
use std::fmt::{Debug, Display, Formatter};
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone)]
pub struct L1State {
    pub bridgehub: Arc<Bridgehub<DynProvider>>,
    pub diamond_proxy: Arc<ZkChain<DynProvider>>,
    pub validator_timelock: Address,
    pub last_committed_batch: u64,
    pub last_proved_batch: u64,
    pub last_executed_batch: u64,
    pub da_input_mode: BatchDaInputMode,
}

impl Debug for L1State {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("L1State")
            .field("bridgehub", &self.bridgehub.address())
            .field("diamond_proxy", &self.diamond_proxy.address())
            .field("validator_timelock", &self.validator_timelock)
            .field("last_committed_batch", &self.last_committed_batch)
            .field("last_proved_batch", &self.last_proved_batch)
            .field("last_executed_batch", &self.last_executed_batch)
            .field("da_input_mode", &self.da_input_mode)
            .finish()
    }
}

impl L1State {
    pub fn bridgehub_address(&self) -> Address {
        *self.bridgehub.address()
    }

    pub fn diamond_proxy(&self) -> ZkChain<DynProvider> {
        self.diamond_proxy.as_ref().clone()
    }

    pub fn diamond_proxy_address(&self) -> Address {
        *self.diamond_proxy.address()
    }

    pub fn report_metrics(&self) {
        // Need to leak Strings here as metric exporter expects label names as `&'static`
        // This only happens once per process lifetime so is safe
        let bridgehub: &'static str = self.bridgehub.address().to_string().leak();
        let diamond_proxy: &'static str = self.diamond_proxy.address().to_string().leak();
        let validator_timelock: &'static str = self.validator_timelock.to_string().leak();
        L1_STATE_METRICS.l1_contract_addresses[&(bridgehub, diamond_proxy, validator_timelock)]
            .set(1);

        let da_input_mode: &'static str = match self.da_input_mode {
            BatchDaInputMode::Rollup => "rollup",
            BatchDaInputMode::Validium => "validium",
        };
        L1_STATE_METRICS.da_input_mode[&da_input_mode].set(1);
    }

    /// Waits until pending L1 state is consistent with latest L1 state (i.e. there are no pending
    /// transactions that are modifying our L2 chain state).
    pub async fn wait_to_finalize(
        self,
        provider: impl Provider + Clone,
        chain_id: u64,
    ) -> anyhow::Result<Self> {
        let zk_chain = self.diamond_proxy.as_ref();
        let last_committed_batch =
            wait_to_finalize(|block_id| zk_chain.get_total_batches_committed(block_id))
                .await
                .context("getTotalBatchesCommitted")?;
        let last_proved_batch =
            wait_to_finalize(|block_id| zk_chain.get_total_batches_proved(block_id))
                .await
                .context("getTotalBatchesVerified")?;
        let last_executed_batch =
            wait_to_finalize(|block_id| zk_chain.get_total_batches_executed(block_id))
                .await
                .context("getTotalBatchesExecuted")?;
        Ok(Self {
            bridgehub: self.bridgehub,
            diamond_proxy: self.diamond_proxy,
            validator_timelock: self.validator_timelock,
            last_committed_batch,
            last_proved_batch,
            last_executed_batch,
            da_input_mode: self.da_input_mode,
        })
    }
}

/// Waits until provided function returns consistent values for both `latest` and `pending` block ids.
async fn wait_to_finalize<
    T: PartialEq + tracing::Value + Display,
    Fut: Future<Output = alloy::contract::Result<T>>,
>(
    f: impl Fn(BlockId) -> Fut,
) -> anyhow::Result<T> {
    /// Ethereum blocks are mined every ~12 seconds on average, but we wait in 1-second intervals
    /// optimistically to save time on startup.
    const RETRY_BUILDER: ConstantBuilder = ConstantBuilder::new()
        .with_delay(Duration::from_secs(1))
        .with_max_times(10);

    let pending_value = f(BlockId::pending())
        .await
        .context("failed to get pending value")?;
    // Note: we do not retry networking errors here. We only retry if the pending state is ahead of latest
    // Outer `Result` is used for retries, inner result is propagated as is.
    let result = (|| async {
        let last_value = f(BlockId::latest())
            .await
            .context("failed to get latest value");
        match last_value {
            Ok(last_value) if last_value == pending_value => Ok(Ok(last_value)),
            Ok(last_value) => Err(last_value),
            Err(_) => Ok(last_value),
        }
    })
    .retry(RETRY_BUILDER)
    .notify(|last_value, _| {
        tracing::info!(
            pending_value,
            last_value,
            "encountered a pending state change on L1; waiting for it to finalize"
        );
    })
    .await;

    match result {
        Ok(last_result) => {
            let last_value = last_result?;
            // Sanity-check that the pending state has not changed since we started waiting.
            let pending_value = f(BlockId::pending())
                .await
                .context("failed to get pending value")?;
            if pending_value != last_value {
                Err(anyhow::anyhow!(
                    "pending state changed while waiting for it to finalize; another main node could already be running"
                ))
            } else {
                Ok(last_value)
            }
        }
        Err(last_value) => Err(anyhow::anyhow!(
            "pending state did not finalize in time; last value: {last_value}"
        )),
    }
}

pub async fn get_l1_state(
    provider: DynProvider,
    bridgehub_address: Address,
    chain_id: u64,
) -> anyhow::Result<L1State> {
    let bridgehub = Bridgehub::new(bridgehub_address, provider, chain_id);
    let all_chain_ids = bridgehub.get_all_zk_chain_chain_ids().await?;
    anyhow::ensure!(
        all_chain_ids.contains(&U256::from(chain_id)),
        "chain ID {chain_id} is not registered on L1"
    );
    let diamond_proxy = bridgehub.zk_chain().await?;
    let validator_timelock_address = bridgehub.validator_timelock_address().await?;

    let latest = BlockId::latest();
    let last_committed_batch = diamond_proxy.get_total_batches_committed(latest).await?;
    let last_proved_batch = diamond_proxy.get_total_batches_proved(latest).await?;
    let last_executed_batch = diamond_proxy.get_total_batches_executed(latest).await?;

    let pubdata_pricing_mode = diamond_proxy.get_pubdata_pricing_mode().await?;
    let da_input_mode = match pubdata_pricing_mode {
        PubdataPricingMode::Rollup => BatchDaInputMode::Rollup,
        PubdataPricingMode::Validium => BatchDaInputMode::Validium,
        v => panic!("unexpected pubdata pricing mode: {}", v as u8),
    };

    Ok(L1State {
        bridgehub: Arc::new(bridgehub),
        diamond_proxy: Arc::new(diamond_proxy),
        validator_timelock: validator_timelock_address,
        last_committed_batch,
        last_proved_batch,
        last_executed_batch,
        da_input_mode,
    })
}
