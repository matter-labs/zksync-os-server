use crate::metrics::L1_STATE_METRICS;
use crate::models::BatchDaInputMode;
use crate::{Bridgehub, MultisigCommitter, PubdataPricingMode, ZkChain};
use alloy::eips::BlockId;
use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use anyhow::Context;
use backon::{ConstantBuilder, Retryable};
use std::fmt::Debug;
use std::time::Duration;
use zksync_os_provider::NodeProvider;

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
    pub bridgehub_l1: Bridgehub<NodeProvider>,
    pub diamond_proxy_l1: ZkChain<NodeProvider>,
    pub validator_timelock: Address,
    pub batch_verification: BatchVerificationSL,
    pub last_committed_batch: u64,
    pub last_proved_batch: u64,
    pub last_executed_batch: u64,
    pub last_finalized_executed_batch: u64,
    /// L1 block number that was used to query `last_committed_batch`, `last_proved_batch`, `last_executed_batch`.
    pub l1_block_number: u64,
    /// Finalized L1 block number that was used to query `last_finalized_executed_batch`.
    pub finalized_l1_block_number: u64,
    pub da_input_mode: BatchDaInputMode,
    pub l1_chain_id: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BatchFinality {
    last_committed_batch: u64,
    last_proved_batch: u64,
    last_executed_batch: u64,
}

impl L1State {
    /// Resolves the L1 diamond proxy for this chain via the L1 Bridgehub, without fetching any
    /// batch-finality state.
    ///
    /// Startup uses this to initialize genesis and the repository manager *before* the startup L1
    /// revert (the revert's `from_block_hash` guard reads the current local block hash via the
    /// repository manager). The full [`L1State`] — including the batch-finality numbers
    /// (`last_committed_batch`, ...) that a revert invalidates — is fetched only after the revert
    /// decision point, so no stale batch-finality data can reach the components initialized here.
    pub async fn resolve_diamond_proxy_l1(
        l1_provider: NodeProvider,
        bridgehub_address_l1: Address,
        l2_chain_id: u64,
    ) -> anyhow::Result<ZkChain<NodeProvider>> {
        let (_, diamond_proxy_l1) =
            Self::resolve_l1_bridgehub_and_proxy(l1_provider, bridgehub_address_l1, l2_chain_id)
                .await?;
        Ok(diamond_proxy_l1)
    }

    /// Builds the L1 Bridgehub handle and resolves the chain's L1 diamond proxy through it.
    async fn resolve_l1_bridgehub_and_proxy(
        l1_provider: NodeProvider,
        bridgehub_address_l1: Address,
        l2_chain_id: u64,
    ) -> anyhow::Result<(Bridgehub<NodeProvider>, ZkChain<NodeProvider>)> {
        let bridgehub_l1 = Bridgehub::new(bridgehub_address_l1, l1_provider, l2_chain_id);
        let diamond_proxy_l1 = bridgehub_l1.zk_chain().await?;
        Ok((bridgehub_l1, diamond_proxy_l1))
    }

    /// Fetches L1 ecosystem contracts along with batch finality status as of latest block.
    pub async fn fetch(
        l1_provider: NodeProvider,
        bridgehub_address_l1: Address,
        l2_chain_id: u64,
    ) -> anyhow::Result<Self> {
        let l1_chain_id = l1_provider.get_chain_id().await?;

        let (bridgehub_l1, diamond_proxy_l1) =
            Self::resolve_l1_bridgehub_and_proxy(l1_provider, bridgehub_address_l1, l2_chain_id)
                .await?;

        // `ZKChainStorage::getSettlementLayer()` returns address(0) when the chain settles on L1,
        // or a Gateway diamond proxy address after a migration. Gateway settlement is no longer
        // supported, so anything non-zero is a hard error.
        let settlement_layer_address = diamond_proxy_l1
            .get_settlement_layer(BlockId::latest())
            .await?;
        anyhow::ensure!(
            settlement_layer_address.is_zero(),
            "chain currently settles on Gateway (settlement layer: {settlement_layer_address}); \
             Gateway settlement is no longer supported"
        );

        Self::validate_chain_ids(&bridgehub_l1, l2_chain_id).await?;

        let validator_timelock = bridgehub_l1.validator_timelock_address().await?;

        let latest_l1_block_number = diamond_proxy_l1.provider().get_block_number().await?;
        let last_committed_batch = diamond_proxy_l1
            .get_total_batches_committed(latest_l1_block_number.into())
            .await?;
        let last_proved_batch = diamond_proxy_l1
            .get_total_batches_proved(latest_l1_block_number.into())
            .await?;
        let last_executed_batch = diamond_proxy_l1
            .get_total_batches_executed(latest_l1_block_number.into())
            .await?;
        let (finalized_l1_block_number, last_finalized_executed_batch) =
            fetch_finalized_executed_batch(&diamond_proxy_l1).await?;

        let pubdata_pricing_mode = diamond_proxy_l1.get_pubdata_pricing_mode().await?;
        let da_input_mode = match pubdata_pricing_mode {
            PubdataPricingMode::Rollup => BatchDaInputMode::Rollup,
            PubdataPricingMode::Validium => BatchDaInputMode::Validium,
            v => panic!("unexpected pubdata pricing mode: {}", v as u8),
        };

        let batch_verification = match MultisigCommitter::try_new(
            validator_timelock,
            diamond_proxy_l1.provider().clone(),
            *diamond_proxy_l1.address(),
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
            diamond_proxy_l1,
            validator_timelock,
            batch_verification,
            last_committed_batch,
            last_proved_batch,
            last_executed_batch,
            last_finalized_executed_batch,
            l1_block_number: latest_l1_block_number,
            finalized_l1_block_number,
            da_input_mode,
            l1_chain_id,
        })
    }

    async fn validate_chain_ids(
        bridgehub_l1: &Bridgehub<NodeProvider>,
        l2_chain_id: u64,
    ) -> anyhow::Result<()> {
        let all_chain_ids_l1 = bridgehub_l1.get_all_zk_chain_chain_ids().await?;
        anyhow::ensure!(
            all_chain_ids_l1.contains(&U256::from(l2_chain_id)),
            "chain ID {l2_chain_id} is not registered on L1"
        );

        Ok(())
    }

    /// Equivalent to [`Self::fetch`] that also waits until the pending L1 state is consistent with
    /// the latest L1 state (i.e., there are no pending transactions that are committing / proving /
    /// executing batches on L1).
    ///
    /// NOTE: This should only be called on the main node as ENs will observe pending changes that
    /// are being submitted by the main node.
    pub async fn fetch_finalized(
        l1_provider: NodeProvider,
        bridgehub_address: Address,
        chain_id: u64,
    ) -> anyhow::Result<Self> {
        let this = Self::fetch(l1_provider, bridgehub_address, chain_id).await?;
        let zk_chain = &this.diamond_proxy_l1;
        let (l1_block_number, batch_finality) =
            wait_to_finalize(zk_chain.provider(), |block_id| async move {
                Ok(BatchFinality {
                    last_committed_batch: zk_chain.get_total_batches_committed(block_id).await?,
                    last_proved_batch: zk_chain.get_total_batches_proved(block_id).await?,
                    last_executed_batch: zk_chain.get_total_batches_executed(block_id).await?,
                })
            })
            .await
            .context("failed to fetch finalized batch state")?;
        let (finalized_l1_block_number, last_finalized_executed_batch) =
            fetch_finalized_executed_batch(zk_chain).await?;
        Ok(Self {
            bridgehub_l1: this.bridgehub_l1,
            diamond_proxy_l1: this.diamond_proxy_l1,
            validator_timelock: this.validator_timelock,
            batch_verification: this.batch_verification,
            last_committed_batch: batch_finality.last_committed_batch,
            last_proved_batch: batch_finality.last_proved_batch,
            last_executed_batch: batch_finality.last_executed_batch,
            last_finalized_executed_batch,
            l1_block_number,
            finalized_l1_block_number,
            da_input_mode: this.da_input_mode,
            l1_chain_id: this.l1_chain_id,
        })
    }

    /// Fetch L1 state, optionally waiting for all pending L1 transactions to finalize first.
    pub async fn fetch_with_finality(
        use_finalized: bool,
        l1_provider: NodeProvider,
        bridgehub_address: Address,
        chain_id: u64,
    ) -> anyhow::Result<Self> {
        if use_finalized {
            Self::fetch_finalized(l1_provider, bridgehub_address, chain_id).await
        } else {
            Self::fetch(l1_provider, bridgehub_address, chain_id).await
        }
    }

    pub fn diamond_proxy_address(&self) -> Address {
        *self.diamond_proxy_l1.address()
    }

    pub fn report_metrics(&self) {
        // Need to leak Strings here as metric exporter expects label names as `&'static`
        // This only happens once per process lifetime so is safe
        let bridgehub: &'static str = self.bridgehub_l1.address().to_string().leak();
        let diamond_proxy: &'static str = self.diamond_proxy_l1.address().to_string().leak();
        let validator_timelock: &'static str = self.validator_timelock.to_string().leak();
        L1_STATE_METRICS.l1_contract_addresses[&(bridgehub, diamond_proxy, validator_timelock)]
            .set(1);

        let da_input_mode: &'static str = match self.da_input_mode {
            BatchDaInputMode::Rollup => "rollup",
            BatchDaInputMode::Validium => "validium",
        };
        L1_STATE_METRICS.da_input_mode[&da_input_mode].set(1);
    }
}

async fn fetch_finalized_executed_batch(
    zk_chain: &ZkChain<NodeProvider>,
) -> anyhow::Result<(u64, u64)> {
    let finalized_l1_block_number = zk_chain
        .provider()
        .get_block_number_by_id(BlockId::finalized())
        .await
        .context("failed to fetch finalized L1 block number")?
        .context("failed to fetch finalized L1 block number (`None` returned)")?;

    if !zk_chain
        .code_exists_at_block(finalized_l1_block_number.into())
        .await
        .context("failed to check ZK chain contract code at finalized L1 block")?
    {
        return Ok((finalized_l1_block_number, 0));
    }

    let last_finalized_executed_batch = zk_chain
        .get_total_batches_executed(finalized_l1_block_number.into())
        .await?;
    Ok((finalized_l1_block_number, last_finalized_executed_batch))
}

/// Waits until the pending L1 state matches the latest finalized L1 block.
async fn wait_to_finalize<T: Debug + PartialEq, Fut: Future<Output = crate::Result<T>>>(
    provider: &NodeProvider,
    f: impl Fn(BlockId) -> Fut,
) -> anyhow::Result<(u64, T)> {
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
        let latest_block_number = provider
            .get_block_number()
            .await
            .context("failed to get latest block number");
        let latest_block_number = match latest_block_number {
            Ok(latest_block_number) => latest_block_number,
            Err(err) => return Ok(Err(err)),
        };
        let last_value = f(latest_block_number.into())
            .await
            .context("failed to get latest value");
        match last_value {
            Ok(last_value) if last_value == pending_value => {
                Ok(Ok((latest_block_number, last_value)))
            }
            Ok(last_value) => Err((latest_block_number, last_value)),
            Err(err) => Ok(Err(err)),
        }
    })
    .retry(RETRY_BUILDER)
    .notify(|(latest_block_number, last_value), _| {
        tracing::info!(
            pending_value = ?pending_value,
            latest_block_number,
            latest_value = ?last_value,
            "encountered a pending L1 state change; waiting for it to finalize"
        );
    })
    .await;

    match result {
        Ok(last_result) => {
            let (latest_block_number, last_value) = last_result?;
            // Sanity-check that the pending state has not changed since we started waiting.
            let pending_value = f(BlockId::pending())
                .await
                .context("failed to get pending value")?;
            if pending_value != last_value {
                Err(anyhow::anyhow!(
                    "pending state changed while waiting for it to finalize; another main node could already be running"
                ))
            } else {
                Ok((latest_block_number, last_value))
            }
        }
        Err((latest_block_number, last_value)) => Err(anyhow::anyhow!(
            "pending state did not finalize in time at L1 block {latest_block_number}; last value: {last_value:?}"
        )),
    }
}
