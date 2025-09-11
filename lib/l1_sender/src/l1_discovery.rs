use crate::config::BatchDaInputMode;
use alloy::eips::BlockId;
use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use anyhow::Context;
use backon::{ConstantBuilder, Retryable};
use std::fmt::Display;
use std::time::Duration;
use zksync_os_contract_interface::{Bridgehub, PubdataPricingMode};

#[derive(Debug, Clone)]
pub struct L1State {
    pub bridgehub: Address,
    pub diamond_proxy: Address,
    pub validator_timelock: Address,
    pub last_committed_batch: u64,
    pub last_proved_batch: u64,
    pub last_executed_batch: u64,
    pub da_input_mode: BatchDaInputMode,
}

/// Waits until pending L1 state is consistent with latest L1 state (i.e. there are no pending
/// transactions that are modifying our L2 chain state).
async fn wait_to_finalize<
    T: PartialEq + tracing::Value + Display,
    Fut: Future<Output = alloy::contract::Result<T>>,
>(
    f: impl Fn(BlockId) -> Fut,
) -> anyhow::Result<T> {
    /// Ethereum blocks are mined every ~12 seconds on average, so we wait in 6-second intervals
    /// optimistically.
    const RETRY_BUILDER: ConstantBuilder = ConstantBuilder::new()
        .with_delay(Duration::from_secs(6))
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
    provider: impl Provider + Clone,
    is_main_node: bool,
    bridgehub_address: Address,
    chain_id: u64,
) -> anyhow::Result<L1State> {
    let bridgehub = Bridgehub::new(bridgehub_address, provider, chain_id);
    let all_chain_ids = bridgehub.get_all_zk_chain_chain_ids().await?;
    anyhow::ensure!(
        all_chain_ids.contains(&U256::from(chain_id)),
        "chain ID {} is not registered on L1",
        chain_id
    );
    let zk_chain = bridgehub.zk_chain().await?;
    let diamond_proxy = *zk_chain.address();
    let validator_timelock_address = bridgehub.validator_timelock_address().await?;

    // If this is a main node, we need to wait for the pending chain state to finalize before proceeding.
    let last_committed_batch = if is_main_node {
        wait_to_finalize(|block_id| zk_chain.get_total_batches_committed(block_id))
            .await
            .context("getTotalBatchesCommitted")?
    } else {
        zk_chain
            .get_total_batches_committed(BlockId::latest())
            .await?
    };
    let last_proved_batch = if is_main_node {
        wait_to_finalize(|block_id| zk_chain.get_total_batches_proved(block_id))
            .await
            .context("getTotalBatchesVerified")?
    } else {
        zk_chain.get_total_batches_proved(BlockId::latest()).await?
    };
    let last_executed_batch = if is_main_node {
        wait_to_finalize(|block_id| zk_chain.get_total_batches_executed(block_id))
            .await
            .context("getTotalBatchesExecuted")?
    } else {
        zk_chain
            .get_total_batches_executed(BlockId::latest())
            .await?
    };

    let pubdata_pricing_mode = zk_chain.get_pubdata_pricing_mode().await?;
    let da_input_mode = match pubdata_pricing_mode {
        PubdataPricingMode::Rollup => BatchDaInputMode::Rollup,
        PubdataPricingMode::Validium => BatchDaInputMode::Validium,
        v => panic!("Unexpected pubdata pricing mode: {}", v as u8),
    };

    Ok(L1State {
        bridgehub: bridgehub_address,
        diamond_proxy,
        validator_timelock: validator_timelock_address,
        last_committed_batch,
        last_proved_batch,
        last_executed_batch,
        da_input_mode,
    })
}
