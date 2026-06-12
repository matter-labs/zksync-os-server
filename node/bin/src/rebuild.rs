use anyhow::Context as _;
use ruint::aliases::U256;
use zksync_os_contract_interface::IValidatorTimelock;
use zksync_os_contract_interface::l1_discovery::L1State;
use zksync_os_l1_watcher::fetch_batch;
use zksync_os_operator_signer::SignerConfig;
use zksync_os_provider::{EthWalletProvider, NodeProvider};
use zksync_os_storage_api::ReadRepository;

use crate::config::{Config, RebuildConfig};

/// What the startup rebuild/revert config asks us to do, decided by [`plan_startup_rebuild`]
/// before any L1 transaction is sent.
enum RebuildAction {
    /// The `from_block_hash` guard says the operation already ran (or `from_block` is unknown
    /// locally): drop the rebuild config so the local block rebuild is skipped too.
    SkipAndClearConfig,
    /// Nothing to do on L1: either a local-only `block_rebuild`, or a standalone `l1_revert`
    /// whose skip conditions hold (already reverted / hash mismatch).
    NoL1Revert,
    /// Revert all committed batches above `last_l1_batch_to_keep` on the settlement layer.
    RevertL1 {
        last_l1_batch_to_keep: u64,
        l1_reverter_sk: SignerConfig,
    },
}

/// Derives `last_l1_batch_to_keep` from `from_block` by scanning committed-only batches on L1.
///
/// Returns an error if:
/// - there are no committed batches on L1,
/// - all committed batches are already executed (finalized),
/// - `from_block` is beyond the last committed block (no batch to revert), or
/// - `from_block` lies within an executed (finalized) batch.
async fn derive_last_l1_batch_to_keep(
    from_block: u64,
    l1_state: &L1State,
    max_blocks_to_process: u64,
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
        fetch_batch(&l1_state.diamond_proxy_sl, batch, max_blocks_to_process)
            .await
            .with_context(|| format!("failed to fetch committed batch {batch} from L1"))
    };

    // Precondition: from_block must not be past the tip of the last committed batch.
    let top = fetch_committed(last_committed_batch).await?;
    anyhow::ensure!(
        from_block <= top.last_block_number(),
        "from_block ({from_block}) is beyond the last committed batch {last_committed_batch} \
         (blocks {}..={}); nothing to revert",
        top.first_block_number(),
        top.last_block_number(),
    );

    // The first batch (scanning from the top) that starts at or before `from_block` is the
    // batch containing it — i.e. the first batch to revert; last_to_keep is one below it.
    if top.first_block_number() <= from_block {
        return Ok(last_committed_batch - 1);
    }
    for batch in (last_executed_batch + 1..last_committed_batch).rev() {
        if fetch_committed(batch).await?.first_block_number() <= from_block {
            return Ok(batch - 1);
        }
    }

    anyhow::bail!(
        "from_block ({from_block}) is at or before the first committed-only batch \
         ({}); it lies within an executed (finalized) batch and cannot be reverted",
        last_executed_batch + 1,
    );
}

/// Calls `revertBatchesSharedBridge` on the validator timelock to roll back all committed batches
/// above `last_l1_batch_to_keep`. Verifies the reverter has the required role before submitting.
///
/// Reverting executed (finalized) batches is impossible: [`plan_startup_rebuild`] checks the
/// target against the on-chain `last_executed_batch`, and the Executor contract itself rejects
/// reverts below `totalBatchesExecuted`.
async fn perform_l1_revert(
    last_l1_batch_to_keep: u64,
    l1_state: &L1State,
    chain_id: u64,
    sl_provider: &NodeProvider,
    reverter_sk: &SignerConfig,
) -> anyhow::Result<()> {
    let mut sl_provider = sl_provider.clone();

    let reverter_address = reverter_sk
        .register_with_wallet(sl_provider.wallet_mut())
        .await
        .context("failed to initialize `sequencer.rebuild.l1_reverter_sk`")?;

    let validator_timelock = IValidatorTimelock::new(l1_state.validator_timelock_sl, sl_provider);
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
        last_l1_batch_to_keep,
        current_last_committed_batch = l1_state.last_committed_batch,
        current_last_executed_batch = l1_state.last_executed_batch,
        reverter = %reverter_address,
        validator_timelock = %l1_state.validator_timelock_sl,
        "performing startup L1 revert"
    );

    let revert_tx = validator_timelock
        .revertBatchesSharedBridge(
            *l1_state.diamond_proxy_sl.address(),
            U256::from(last_l1_batch_to_keep),
        )
        .from(reverter_address)
        .send()
        .await
        .with_context(|| {
            format!(
                "failed to submit `revertBatchesSharedBridge` to validator timelock {}",
                l1_state.validator_timelock_sl
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
        last_l1_batch_to_keep,
        "startup L1 revert completed"
    );

    Ok(())
}

/// Checks whether block `from_block` currently has the expected `from_block_hash`.
///
/// Returns `false` (operation should be skipped) when the hashes differ — distinguishing the two
/// reasons in the logs:
/// - block missing locally: likely a misconfigured `from_block` (typo / beyond local tip);
/// - hash changed: the rebuild/revert already ran on a previous startup (the expected case).
fn from_block_hash_matches(
    repositories: &dyn ReadRepository,
    from_block: u64,
    from_block_hash: alloy::primitives::BlockHash,
) -> bool {
    let current_hash = repositories
        .get_block_by_number(from_block)
        .ok()
        .flatten()
        .map(|b| b.hash());
    match current_hash {
        Some(hash) if hash == from_block_hash => true,
        Some(hash) => {
            tracing::info!(
                from_block,
                current_hash = ?hash,
                ?from_block_hash,
                "skipping startup rebuild/revert: from_block_hash changed (already ran)"
            );
            false
        }
        None => {
            tracing::warn!(
                from_block,
                ?from_block_hash,
                "skipping startup rebuild/revert: block `from_block` not found locally \
                 (check `from_block` is correct — it may be a typo or beyond the local tip)"
            );
            false
        }
    }
}

/// Decides what to do for the given rebuild config without performing any L1 transaction.
async fn plan_startup_rebuild(
    rebuild: &RebuildConfig,
    repositories: &dyn ReadRepository,
    l1_state: &L1State,
    max_blocks_to_process: u64,
) -> anyhow::Result<RebuildAction> {
    match rebuild {
        RebuildConfig::BlockRebuild { bounds } => Ok(
            if from_block_hash_matches(repositories, bounds.from_block, bounds.from_block_hash) {
                // No L1 revert for this mode.
                RebuildAction::NoL1Revert
            } else {
                RebuildAction::SkipAndClearConfig
            },
        ),

        RebuildConfig::DangerBlockRebuildWithL1Revert {
            bounds,
            l1_reverter_sk,
        } => {
            if !from_block_hash_matches(repositories, bounds.from_block, bounds.from_block_hash) {
                return Ok(RebuildAction::SkipAndClearConfig);
            }

            tracing::warn!(
                from_block = bounds.from_block,
                last_committed_batch = l1_state.last_committed_batch,
                "DangerBlockRebuildWithL1Revert: deriving batch to revert from from_block"
            );

            let last_l1_batch_to_keep =
                derive_last_l1_batch_to_keep(bounds.from_block, l1_state, max_blocks_to_process)
                    .await
                    .context("failed to derive last_l1_batch_to_keep")?;

            Ok(RebuildAction::RevertL1 {
                last_l1_batch_to_keep,
                l1_reverter_sk: l1_reverter_sk.clone(),
            })
        }

        RebuildConfig::L1Revert {
            from_batch,
            from_batch_hash,
            l1_reverter_sk,
        } => {
            let from_batch = *from_batch;
            anyhow::ensure!(
                from_batch >= 1,
                "`l1_revert.from_batch` must be >= 1 (batch 0 is genesis and cannot be reverted)"
            );

            if l1_state.last_committed_batch < from_batch {
                tracing::info!(
                    from_batch,
                    last_committed_batch = l1_state.last_committed_batch,
                    "skipping L1Revert: already reverted or no batches to revert"
                );
                return Ok(RebuildAction::NoL1Revert);
            }

            anyhow::ensure!(
                from_batch > l1_state.last_executed_batch,
                "`l1_revert.from_batch` ({from_batch}) is at or before the last executed batch \
                 ({}); executed batches are finalized on L1 and cannot be reverted",
                l1_state.last_executed_batch,
            );

            let on_chain_hash = fetch_batch(
                &l1_state.diamond_proxy_sl,
                from_batch,
                max_blocks_to_process,
            )
            .await
            .context("failed to fetch on-chain hash for L1Revert from_batch")?
            .batch_info
            .hash();

            if on_chain_hash != *from_batch_hash {
                tracing::info!(
                    from_batch,
                    ?on_chain_hash,
                    ?from_batch_hash,
                    "skipping L1Revert: from_batch_hash mismatch"
                );
                return Ok(RebuildAction::NoL1Revert);
            }

            tracing::warn!(
                from_batch,
                last_l1_batch_to_keep = from_batch - 1,
                last_committed_batch = l1_state.last_committed_batch,
                "L1Revert: performing standalone L1 revert"
            );

            Ok(RebuildAction::RevertL1 {
                last_l1_batch_to_keep: from_batch - 1,
                l1_reverter_sk: l1_reverter_sk.clone(),
            })
        }
    }
}

/// Handles the startup rebuild/revert config and performs L1 revert.
///
/// Returns `true` if an L1 revert was performed and the caller should refresh `L1State`.
/// Returns `false` if no revert was needed (skipped or `BlockRebuild` which never touches L1).
pub async fn handle_startup_rebuild(
    config: &mut Config,
    repositories: &dyn ReadRepository,
    l1_state: &L1State,
    sl_provider: &NodeProvider,
) -> anyhow::Result<bool> {
    let Some(rebuild) = config.sequencer_config.rebuild.clone() else {
        return Ok(false);
    };
    let chain_id = config
        .genesis_config
        .chain_id
        .context("`genesis.chain_id` is required for startup rebuild")?;
    let max_blocks = config.l1_watcher_config.max_blocks_to_process;

    match plan_startup_rebuild(&rebuild, repositories, l1_state, max_blocks).await? {
        RebuildAction::SkipAndClearConfig => {
            config.sequencer_config.rebuild = None;
            Ok(false)
        }
        RebuildAction::NoL1Revert => Ok(false),
        RebuildAction::RevertL1 {
            last_l1_batch_to_keep,
            l1_reverter_sk,
        } => {
            perform_l1_revert(
                last_l1_batch_to_keep,
                l1_state,
                chain_id,
                sl_provider,
                &l1_reverter_sk,
            )
            .await
            .context("failed to perform startup L1 revert")?;
            Ok(true)
        }
    }
}
