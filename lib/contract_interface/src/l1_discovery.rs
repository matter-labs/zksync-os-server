use crate::metrics::L1_STATE_METRICS;
use crate::models::BatchDaInputMode;
use crate::{Bridgehub, MultisigCommitter, PubdataPricingMode, ZkChain};
use alloy::eips::BlockId;
use alloy::primitives::{Address, U256, address};
use alloy::providers::{DynProvider, Provider};
use anyhow::Context;
use backon::{ConstantBuilder, Retryable};
use std::fmt::{Debug, Display};
use std::time::Duration;

const L2_BRIDGEHUB_ADDRESS: Address = address!("0x0000000000000000000000000000000000010002");

#[derive(Clone, Debug)]
pub struct BatchVerificationSLConfig {
    pub threshold: u64,
    pub validators: Vec<Address>,
}

#[derive(Clone, Debug)]
pub enum BatchVerificationSL {
    Disabled,
    Enabled(BatchVerificationSLConfig),
}

#[derive(Clone, Debug)]
pub struct L1State {
    pub bridgehub_l1: Bridgehub<DynProvider>,
    pub bridgehub_sl: Bridgehub<DynProvider>,
    pub diamond_proxy_l1: ZkChain<DynProvider>,
    pub diamond_proxy_sl: ZkChain<DynProvider>,
    pub validator_timelock_sl: Address,
    pub batch_verification: BatchVerificationSL,
    pub last_committed_batch: u64,
    pub last_proved_batch: u64,
    pub last_executed_batch: u64,
    pub da_input_mode: BatchDaInputMode,
    pub l1_chain_id: u64,
    pub sl_chain_id: u64,
}

impl L1State {
    /// Fetches L1 ecosystem contracts along with batch finality status as of latest block.
    ///
    /// `gateway_provider` must be `Some` when the chain is settling on the Gateway and `None`
    /// when settling on L1. An error is returned if the chain is found to be on the Gateway but
    /// no provider was supplied.
    pub async fn fetch(
        l1_provider: DynProvider,
        gateway_provider: Option<DynProvider>,
        bridgehub_address_l1: Address,
        l2_chain_id: u64,
    ) -> anyhow::Result<Self> {
        let l1_chain_id = l1_provider.get_chain_id().await?;

        let bridgehub_l1 = Bridgehub::new(bridgehub_address_l1, l1_provider, l2_chain_id);
        let diamond_proxy_l1 = bridgehub_l1.zk_chain().await?;

        // Call ZKChainStorage::getSettlementLayer() on the L1 diamond proxy to determine whether
        // this chain is currently settling on L1 or on the Gateway.
        // Returns address(0) when settling on L1, or the Gateway diamond proxy address after migration.
        let settlement_layer_address = diamond_proxy_l1.get_settlement_layer().await?;

        let (sl_chain_id, bridgehub_sl) = if settlement_layer_address.is_zero() {
            // Settling on L1: the settlement layer is L1 itself.
            (l1_chain_id, bridgehub_l1.clone())
        } else {
            // Settling on Gateway: require a dedicated Gateway RPC provider.
            let gateway_provider = gateway_provider.with_context(|| {
                format!(
                    "chain is settling on Gateway (settlement layer: {settlement_layer_address}) \
                     but no gateway RPC URL is configured"
                )
            })?;
            let sl_chain_id = gateway_provider.get_chain_id().await?;
            anyhow::ensure!(
                sl_chain_id != l1_chain_id,
                "settling on Gateway but SL chain ID is identical to L1 chain ID"
            );
            let bridgehub_sl = Bridgehub::new(L2_BRIDGEHUB_ADDRESS, gateway_provider, l2_chain_id);
            (sl_chain_id, bridgehub_sl)
        };

        Self::validate_chain_ids(&bridgehub_l1, &bridgehub_sl, l2_chain_id).await?;

        let diamond_proxy_sl = bridgehub_sl.zk_chain().await?;
        let validator_timelock_sl = bridgehub_sl.validator_timelock_address().await?;

        let latest = BlockId::latest();
        let last_committed_batch = diamond_proxy_sl.get_total_batches_committed(latest).await?;
        let last_proved_batch = diamond_proxy_sl.get_total_batches_proved(latest).await?;
        let last_executed_batch = diamond_proxy_sl.get_total_batches_executed(latest).await?;

        let pubdata_pricing_mode = diamond_proxy_sl.get_pubdata_pricing_mode().await?;
        let da_input_mode = match pubdata_pricing_mode {
            PubdataPricingMode::Rollup => BatchDaInputMode::Rollup,
            PubdataPricingMode::Validium => BatchDaInputMode::Validium,
            v => panic!("unexpected pubdata pricing mode: {}", v as u8),
        };

        let batch_verification = match MultisigCommitter::try_new(
            validator_timelock_sl,
            diamond_proxy_sl.provider().clone(),
            *diamond_proxy_sl.address(),
        )
        .await
        .context("failed to check MultisigCommitter interface")?
        {
            Some(multisig_committer) => {
                let threshold = multisig_committer
                    .get_signing_threshold()
                    .await
                    .context("failed to get signing threshold")?;
                let validators = multisig_committer
                    .get_validators()
                    .await
                    .context("failed to get validators")?;
                BatchVerificationSL::Enabled(BatchVerificationSLConfig {
                    threshold,
                    validators,
                })
            }
            None => BatchVerificationSL::Disabled,
        };

        Ok(Self {
            bridgehub_l1,
            bridgehub_sl,
            diamond_proxy_l1,
            diamond_proxy_sl,
            validator_timelock_sl,
            batch_verification,
            last_committed_batch,
            last_proved_batch,
            last_executed_batch,
            da_input_mode,
            l1_chain_id,
            sl_chain_id,
        })
    }

    async fn validate_chain_ids(
        bridgehub_l1: &Bridgehub<DynProvider>,
        bridgehub_sl: &Bridgehub<DynProvider>,
        l2_chain_id: u64,
    ) -> anyhow::Result<()> {
        let all_chain_ids_l1 = bridgehub_l1.get_all_zk_chain_chain_ids().await?;
        let all_chain_ids_sl = bridgehub_sl.get_all_zk_chain_chain_ids().await?;
        anyhow::ensure!(
            all_chain_ids_l1.contains(&U256::from(l2_chain_id)),
            "chain ID {l2_chain_id} is not registered on L1"
        );
        anyhow::ensure!(
            all_chain_ids_sl.contains(&U256::from(l2_chain_id)),
            "chain ID {l2_chain_id} is not registered on SL"
        );

        let sl_chain_id = bridgehub_sl.instance.provider().get_chain_id().await?;
        let is_sl_whitelisted = bridgehub_l1
            .whitelisted_settlement_layers(U256::from(sl_chain_id))
            .await?;
        anyhow::ensure!(is_sl_whitelisted, "SL is not whitelisted on L1 Bridgehub");

        Ok(())
    }

    /// Equivalent to [`Self::fetch`] that also waits until the pending L1 state is consistent with the
    /// latest L1 state (i.e., there are no pending transactions that are committing/proving/executing
    /// batches on L1).
    ///
    /// NOTE: This should only be called on the main node as ENs will observe pending changes that
    /// are being submitted by the main node.
    pub async fn fetch_finalized(
        l1_provider: DynProvider,
        gateway_provider: Option<DynProvider>,
        bridgehub_address: Address,
        chain_id: u64,
    ) -> anyhow::Result<Self> {
        let this = Self::fetch(l1_provider, gateway_provider, bridgehub_address, chain_id).await?;
        let zk_chain_sl = &this.diamond_proxy_sl;
        let last_committed_batch =
            wait_to_finalize(|block_id| zk_chain_sl.get_total_batches_committed(block_id))
                .await
                .context("getTotalBatchesCommitted")?;
        let last_proved_batch =
            wait_to_finalize(|block_id| zk_chain_sl.get_total_batches_proved(block_id))
                .await
                .context("getTotalBatchesVerified")?;
        let last_executed_batch =
            wait_to_finalize(|block_id| zk_chain_sl.get_total_batches_executed(block_id))
                .await
                .context("getTotalBatchesExecuted")?;
        Ok(Self {
            bridgehub_l1: this.bridgehub_l1,
            bridgehub_sl: this.bridgehub_sl,
            diamond_proxy_l1: this.diamond_proxy_l1,
            diamond_proxy_sl: this.diamond_proxy_sl,
            validator_timelock_sl: this.validator_timelock_sl,
            batch_verification: this.batch_verification,
            last_committed_batch,
            last_proved_batch,
            last_executed_batch,
            da_input_mode: this.da_input_mode,
            l1_chain_id: this.l1_chain_id,
            sl_chain_id: this.sl_chain_id,
        })
    }

    pub fn diamond_proxy_address_sl(&self) -> Address {
        *self.diamond_proxy_sl.address()
    }

    pub fn report_metrics(&self) {
        // Need to leak Strings here as metric exporter expects label names as `&'static`
        // This only happens once per process lifetime so is safe
        let bridgehub: &'static str = self.bridgehub_l1.address().to_string().leak();
        let diamond_proxy: &'static str = self.diamond_proxy_l1.address().to_string().leak();
        let validator_timelock: &'static str = self.validator_timelock_sl.to_string().leak();
        L1_STATE_METRICS.l1_contract_addresses[&(bridgehub, diamond_proxy, validator_timelock)]
            .set(1);

        let da_input_mode: &'static str = match self.da_input_mode {
            BatchDaInputMode::Rollup => "rollup",
            BatchDaInputMode::Validium => "validium",
        };
        L1_STATE_METRICS.da_input_mode[&da_input_mode].set(1);
    }
}

/// Waits until provided function returns consistent values for both `latest` and `pending` block ids.
async fn wait_to_finalize<
    T: PartialEq + tracing::Value + Display,
    Fut: Future<Output = crate::Result<T>>,
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
