use alloy::eips::BlockId;
use alloy::primitives::U256;
use anyhow::Context as _;
use backon::{ConstantBuilder, Retryable};
use std::time::Duration;
use zksync_os_contract_interface::l1_discovery::L1State;
use zksync_os_contract_interface::{IValidatorTimelock, ZkChain};
use zksync_os_l1_watcher::fetch_live_committed_batch;
use zksync_os_operator_signer::SignerConfig;
use zksync_os_provider::{EthWalletProvider, NodeProvider};

use crate::config::{Config, RebuildConfig};

/// An L1 revert to perform, decided by [`plan_l1_revert`] before any L1 transaction is sent.
/// `plan_l1_revert` returns `None` when no L1 revert is needed.
struct RevertPlan {
    /// Revert all committed batches above this number on the settlement layer.
    last_l1_batch_to_keep: u64,
    l1_reverter_sk: SignerConfig,
}

/// Derives `last_l1_batch_to_keep` from `from_block_number` by scanning committed-only batches on L1.
///
/// Returns an error if:
/// - there are no committed batches on L1,
/// - all committed batches are already executed (finalized),
/// - `from_block_number` is beyond the last committed block (no batch to revert), or
/// - `from_block_number` lies within an executed (finalized) batch.
async fn derive_last_l1_batch_to_keep(
    from_block_number: u64,
    l1_state: &L1State,
) -> anyhow::Result<u64> {
    let last_committed_batch = l1_state.last_committed_batch;
    let last_executed_batch = l1_state.last_executed_batch;
    anyhow::ensure!(
        last_committed_batch > 0,
        "no committed batches on L1; nothing to revert"
    );
    anyhow::ensure!(
        last_committed_batch > last_executed_batch,
        "all committed batches are already executed (finalized); nothing to revert"
    );

    let fetch_committed = |batch: u64| async move {
        fetch_live_committed_batch(&l1_state.diamond_proxy_l1, batch)
            .await
            .map(|(batch_data, _commit_tx_hash)| batch_data)
            .with_context(|| format!("failed to fetch committed batch {batch} from L1"))
    };

    // Precondition: from_block_number must not be past the tip of the last committed batch.
    let top = fetch_committed(last_committed_batch).await?;
    anyhow::ensure!(
        from_block_number <= top.last_block_number(),
        "from_block_number ({from_block_number}) is beyond the last committed batch {last_committed_batch} \
         (blocks {}..={}); nothing to revert",
        top.first_block_number(),
        top.last_block_number(),
    );

    // Fast path for the common revert case: `from_block_number` lies in the last committed batch
    // (reverting from a recent block).
    if top.first_block_number() <= from_block_number {
        return Ok(last_committed_batch - 1);
    }

    // Binary-search the remaining committed-only batches.
    let mut low_batch = last_executed_batch + 1;
    let mut high_batch = last_committed_batch - 1;
    let mut first_batch_to_revert: Option<u64> = None;
    while low_batch <= high_batch {
        let mid_batch = low_batch + (high_batch - low_batch) / 2;
        let mid_first_block = fetch_committed(mid_batch).await?.first_block_number();
        if mid_first_block <= from_block_number {
            first_batch_to_revert = Some(mid_batch);
            low_batch = mid_batch + 1;
        } else {
            high_batch = mid_batch - 1;
        }
    }

    match first_batch_to_revert {
        Some(batch) => Ok(batch - 1),
        None => anyhow::bail!(
            "from_block_number ({from_block_number}) is at or before the first committed-only batch ({}); it \
             lies within an executed (finalized) batch and cannot be reverted",
            last_executed_batch + 1,
        ),
    }
}

/// Calls `revertBatchesSharedBridge` on the validator timelock to roll back all committed batches
/// above `last_l1_batch_to_keep`. Verifies the reverter has the required role before submitting.
///
/// Reverting executed (finalized) batches is impossible: [`plan_l1_revert`] checks the
/// target against the on-chain `last_executed_batch`, and the Executor contract itself rejects
/// reverts below `totalBatchesExecuted`.
async fn perform_l1_revert(
    plan: &RevertPlan,
    l1_state: &L1State,
    chain_id: u64,
    l1_provider: &NodeProvider,
) -> anyhow::Result<()> {
    let mut l1_provider = l1_provider.clone();

    let reverter_address = plan
        .l1_reverter_sk
        .register_with_wallet(l1_provider.wallet_mut())
        .await
        .context("failed to initialize `sequencer.rebuild.l1_reverter_sk`")?;

    let validator_timelock = IValidatorTimelock::new(l1_state.validator_timelock, l1_provider);
    let reverter_role = validator_timelock.REVERTER_ROLE().call().await?;
    let has_reverter_role = validator_timelock
        .hasRoleForChainId(U256::from(chain_id), reverter_role, reverter_address)
        .call()
        .await?;
    anyhow::ensure!(
        has_reverter_role,
        "`sequencer.rebuild.l1_reverter_sk` address {reverter_address} does not have REVERTER_ROLE for chain {chain_id}"
    );

    tracing::warn!(
        last_l1_batch_to_keep = plan.last_l1_batch_to_keep,
        current_last_committed_batch = l1_state.last_committed_batch,
        current_last_executed_batch = l1_state.last_executed_batch,
        reverter = %reverter_address,
        validator_timelock = %l1_state.validator_timelock,
        "performing startup L1 revert"
    );

    let revert_tx = validator_timelock
        .revertBatchesSharedBridge(
            *l1_state.diamond_proxy_l1.address(),
            U256::from(plan.last_l1_batch_to_keep),
        )
        .from(reverter_address)
        .send()
        .await
        .with_context(|| {
            format!(
                "failed to submit `revertBatchesSharedBridge` to validator timelock {}",
                l1_state.validator_timelock
            )
        })?;

    let receipt = revert_tx
        .get_receipt()
        .await
        .context("failed to wait for startup L1 revert receipt")?;
    anyhow::ensure!(
        receipt.status(),
        "startup L1 revert transaction {} failed on-chain",
        receipt.transaction_hash
    );

    tracing::info!(
        tx_hash = ?receipt.transaction_hash,
        l1_block = ?receipt.block_number,
        last_l1_batch_to_keep = plan.last_l1_batch_to_keep,
        "startup L1 revert completed"
    );

    // Ensure L1 node returns the proper last committed batch number after the revert.
    ensure_revert(&l1_state.diamond_proxy_l1, plan.last_l1_batch_to_keep).await?;

    Ok(())
}

/// Waits until the settlement layer reports a committed-batch count consistent with the revert.
async fn ensure_revert(
    diamond_proxy_l1: &ZkChain<NodeProvider>,
    last_l1_batch_to_keep: u64,
) -> anyhow::Result<()> {
    const RETRY_BUILDER: ConstantBuilder = ConstantBuilder::new()
        .with_delay(Duration::from_secs(2))
        .with_max_times(30);

    let observed = match (|| async {
        let committed = diamond_proxy_l1
            .get_total_batches_committed(BlockId::latest())
            .await
            .context("failed to read totalBatchesCommitted")?;
        anyhow::ensure!(
            committed == last_l1_batch_to_keep,
            "last committed batch is {committed}, expected == {last_l1_batch_to_keep}"
        );
        anyhow::Ok(committed)
    })
    .retry(RETRY_BUILDER)
    .notify(|err, _| {
        tracing::warn!(
            error = %err,
            last_l1_batch_to_keep,
            "L1 revert not yet observable at the latest block; retrying"
        );
    })
    .await
    {
        Ok(committed) => committed,
        Err(err) => {
            tracing::error!(
                error = %err,
                last_l1_batch_to_keep,
                "L1 revert did not become observable within the retry timeout; the SL \
                 RPC kept reporting a committed batch different from the kept batch"
            );
            return Err(err)
                .context("L1 revert not observable at the latest block within the retry timeout");
        }
    };

    tracing::info!(
        last_committed_batch = observed,
        last_l1_batch_to_keep,
        "L1 revert observable: settlement-layer committed batch decreased to the expected value"
    );
    Ok(())
}

/// Decides the L1 revert to perform for the given rebuild config without sending any L1
/// transaction. Returns `None` when no L1 revert is needed.
async fn plan_l1_revert(
    rebuild: &RebuildConfig,
    l1_state: &L1State,
) -> anyhow::Result<Option<RevertPlan>> {
    match rebuild {
        RebuildConfig::BlockRebuild { .. } => Ok(None),

        RebuildConfig::DangerBlockRebuildWithL1Revert {
            bounds,
            l1_reverter_sk,
        } => {
            tracing::warn!(
                from_block_number = bounds.from_block_number,
                last_committed_batch = l1_state.last_committed_batch,
                "DangerBlockRebuildWithL1Revert: deriving batch to revert from from_block_number"
            );

            let last_l1_batch_to_keep =
                derive_last_l1_batch_to_keep(bounds.from_block_number, l1_state)
                    .await
                    .context("failed to derive last_l1_batch_to_keep")?;

            Ok(Some(RevertPlan {
                last_l1_batch_to_keep,
                l1_reverter_sk: l1_reverter_sk.clone(),
            }))
        }

        RebuildConfig::L1Revert {
            from_batch_number,
            from_batch_commit_tx_hash,
            l1_reverter_sk,
        } => {
            let from_batch_number = from_batch_number.get();
            if l1_state.last_committed_batch < from_batch_number {
                tracing::info!(
                    from_batch_number,
                    last_committed_batch = l1_state.last_committed_batch,
                    "skipping L1Revert: already reverted or no batches to revert"
                );
                return Ok(None);
            }

            anyhow::ensure!(
                from_batch_number > l1_state.last_executed_batch,
                "`l1_revert.from_batch_number` ({from_batch_number}) is at or before the last \
                 executed batch ({}); executed batches are finalized on L1 and cannot be reverted",
                l1_state.last_executed_batch,
            );

            let (_, on_chain_commit_tx_hash) =
                fetch_live_committed_batch(&l1_state.diamond_proxy_l1, from_batch_number)
                    .await
                    .context(
                        "failed to fetch on-chain commit tx hash for L1Revert from_batch_number",
                    )?;

            if on_chain_commit_tx_hash != *from_batch_commit_tx_hash {
                tracing::info!(
                    from_batch_number,
                    ?on_chain_commit_tx_hash,
                    ?from_batch_commit_tx_hash,
                    "skipping L1Revert: from_batch_commit_tx_hash mismatch (already reverted and \
                     re-committed, or wrong batch)"
                );
                return Ok(None);
            }

            tracing::warn!(
                from_batch_number,
                last_l1_batch_to_keep = from_batch_number - 1,
                last_committed_batch = l1_state.last_committed_batch,
                "L1Revert: performing standalone L1 revert"
            );

            Ok(Some(RevertPlan {
                last_l1_batch_to_keep: from_batch_number - 1,
                l1_reverter_sk: l1_reverter_sk.clone(),
            }))
        }
    }
}

/// Performs the configured startup L1 revert, if any.
///
/// Returns `true` if an L1 revert was performed.
/// Returns `false` if no revert was needed (`BlockRebuild`, or an `L1Revert` whose skip
/// conditions hold).
pub async fn revert_l1_on_startup(
    rebuild: &RebuildConfig,
    config: &Config,
    l1_state: &L1State,
    l1_provider: &NodeProvider,
) -> anyhow::Result<bool> {
    let chain_id = config
        .genesis_config
        .chain_id
        .context("`genesis.chain_id` is required for startup rebuild")?;

    match plan_l1_revert(rebuild, l1_state).await? {
        None => Ok(false),
        Some(plan) => {
            perform_l1_revert(&plan, l1_state, chain_id, l1_provider)
                .await
                .context("failed to perform startup L1 revert")?;
            Ok(true)
        }
    }
}
