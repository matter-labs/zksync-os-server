use anyhow::Context as _;
use ruint::aliases::U256;
use zksync_os_contract_interface::l1_discovery::L1State;
use zksync_os_contract_interface::{IValidatorTimelock, ZkChain};
use zksync_os_l1_watcher::fetch_batch;
use zksync_os_operator_signer::SignerConfig;
use zksync_os_provider::{EthWalletProvider, NodeProvider};
use zksync_os_storage::db::ExecutedBatchStorage;
use zksync_os_storage_api::{ReadBatch, ReadRepository};

use crate::config::{Config, RebuildConfig};

/// Derives `last_l1_batch_to_keep` from `from_block` by scanning committed-only batches.
///
/// Returns an error if:
/// - there are no committed batches on L1,
/// - all committed batches are already executed (finalized), or
/// - `from_block` is beyond the last committed block (no batch to revert).
async fn derive_last_l1_batch_to_keep(
    from_block: u64,
    last_committed_batch: u64,
    last_executed_batch: u64,
    diamond_proxy_sl: &ZkChain<NodeProvider>,
    max_blocks_to_process: u64,
) -> anyhow::Result<u64> {
    anyhow::ensure!(
        last_committed_batch > 0,
        "no committed batches on L1; nothing to revert"
    );
    anyhow::ensure!(
        last_committed_batch > last_executed_batch,
        "all committed batches are already executed (finalized); nothing to revert"
    );

    let first_committed_unexecuted_batch = last_executed_batch + 1;
    let mut batch = last_committed_batch;

    while batch >= first_committed_unexecuted_batch {
        let committed = fetch_batch(diamond_proxy_sl, batch, max_blocks_to_process)
            .await
            .with_context(|| format!("failed to fetch committed batch {batch} from L1"))?;
        let first_block = committed.first_block_number();

        if first_block <= from_block {
            if batch == last_committed_batch && first_block < from_block {
                // from_block is either inside the last committed batch or beyond it entirely.
                let last_block = committed.last_block_number();
                anyhow::ensure!(
                    from_block <= last_block,
                    "from_block ({from_block}) is beyond the last committed batch {batch} \
                     (blocks {first_block}..={last_block}); nothing to revert"
                );
            }
            // from_block is inside this batch (or exactly at its start).
            // This batch is the first to revert; last_to_keep is one below it.
            return Ok(batch - 1);
        }

        batch -= 1;
    }

    unreachable!("scan exhausted without finding from_block ({from_block})");
}

/// Calls `revertBatchesSharedBridge` on the validator timelock to roll back all committed batches
/// above `last_l1_batch_to_keep`. Verifies the reverter has the required role before submitting.
async fn perform_l1_revert(
    last_l1_batch_to_keep: u64,
    l1_state: &L1State,
    chain_id: u64,
    sl_provider: &NodeProvider,
    reverter_sk: &SignerConfig,
    persistent_batch_storage: &ExecutedBatchStorage,
) -> anyhow::Result<()> {
    // Fail fast: if the first batch to revert is already in executed storage, it has been
    // finalized on L1 and cannot be rolled back.
    let reverted_batch = last_l1_batch_to_keep
        .checked_add(1)
        .expect("last_l1_batch_to_keep overflow");
    let already_executed = persistent_batch_storage.get_batch_by_number(reverted_batch)?;
    anyhow::ensure!(
        already_executed.is_none(),
        "cannot revert batch {reverted_batch}: it is already executed (finalized) on L1"
    );

    let mut sl_provider = sl_provider.clone();

    let reverter_address = reverter_sk
        .register_with_wallet(sl_provider.wallet_mut())
        .await
        .context("failed to initialize `l1_sender.reverter_sk`")?;

    let validator_timelock = IValidatorTimelock::new(l1_state.validator_timelock_sl, sl_provider);
    let reverter_role = validator_timelock.REVERTER_ROLE().call().await?;
    let has_reverter_role = validator_timelock
        .hasRoleForChainId(U256::from(chain_id), reverter_role, reverter_address)
        .call()
        .await?;
    anyhow::ensure!(
        has_reverter_role,
        "`l1_sender.reverter_sk` address {reverter_address} does not have REVERTER_ROLE for chain {chain_id}"
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

/// Handles the startup rebuild/revert config and performs L1 revert.
///
/// Returns `true` if an L1 revert was performed and the caller should refresh `L1State`.
/// Returns `false` if no revert was needed (skipped or `BlockRebuild` which never touches L1).
///
/// May clear `config.sequencer_config.rebuild` when the hash guard fails — this suppresses the
/// rebuild on subsequent restarts after the operation already ran.
pub async fn l1_revert(
    config: &mut Config,
    repositories: &dyn ReadRepository,
    l1_state: &L1State,
    persistent_batch_storage: &ExecutedBatchStorage,
    sl_provider: &NodeProvider,
) -> anyhow::Result<bool> {
    let Some(rebuild) = config.sequencer_config.rebuild.clone() else {
        return Ok(false);
    };

    let chain_id = config
        .genesis_config
        .chain_id
        .expect("`genesis.chain_id` is required");
    let max_blocks = config.l1_watcher_config.max_blocks_to_process;

    match rebuild {
        RebuildConfig::BlockRebuild { bounds } => {
            let current_hash = repositories
                .get_block_by_number(bounds.from_block)
                .ok()
                .flatten()
                .map(|b| b.hash());
            if current_hash != Some(bounds.from_block_hash) {
                tracing::info!(
                    from_block = bounds.from_block,
                    ?current_hash,
                    from_block_hash = ?bounds.from_block_hash,
                    "skipping block rebuild: from_block_hash mismatch (already rebuilt)"
                );
                config.sequencer_config.rebuild = None;
            }
            // No L1 revert for this mode.
            Ok(false)
        }

        RebuildConfig::DangerBlockRebuildWithL1Revert {
            bounds,
            l1_reverter_sk,
        } => {
            let current_hash = repositories
                .get_block_by_number(bounds.from_block)
                .ok()
                .flatten()
                .map(|b| b.hash());
            if current_hash != Some(bounds.from_block_hash) {
                tracing::info!(
                    from_block = bounds.from_block,
                    ?current_hash,
                    from_block_hash = ?bounds.from_block_hash,
                    "skipping rebuild+L1 revert: from_block_hash mismatch (already ran)"
                );
                config.sequencer_config.rebuild = None;
                return Ok(false);
            }

            // Fail fast: from_block must lie beyond all finalized (executed) batches.
            if l1_state.last_executed_batch > 0
                && let Some(last_executed) =
                    persistent_batch_storage.get_batch_by_number(l1_state.last_executed_batch)?
            {
                let last_executed_block = last_executed.last_block_number();
                anyhow::ensure!(
                    bounds.from_block > last_executed_block,
                    "from_block ({}) is at or before the last executed batch {} \
                     (blocks {}..={last_executed_block}); executed batches are finalized on \
                     L1 and cannot be reverted",
                    bounds.from_block,
                    l1_state.last_executed_batch,
                    last_executed.first_block_number(),
                );
            }

            tracing::warn!(
                from_block = bounds.from_block,
                last_committed_batch = l1_state.last_committed_batch,
                "DangerBlockRebuildWithL1Revert: deriving batch to revert from from_block"
            );

            let last_l1_batch_to_keep = derive_last_l1_batch_to_keep(
                bounds.from_block,
                l1_state.last_committed_batch,
                l1_state.last_executed_batch,
                &l1_state.diamond_proxy_sl,
                max_blocks,
            )
            .await
            .context("failed to derive last_l1_batch_to_keep")?;

            perform_l1_revert(
                last_l1_batch_to_keep,
                l1_state,
                chain_id,
                sl_provider,
                &l1_reverter_sk,
                persistent_batch_storage,
            )
            .await
            .context("failed to perform startup L1 revert")?;

            Ok(true)
        }

        RebuildConfig::L1Revert {
            from_batch,
            from_batch_hash,
            l1_reverter_sk,
        } => {
            let last_l1_batch_to_keep = from_batch
                .checked_sub(1)
                .expect("l1_revert.from_batch must be >= 1");

            if l1_state.last_committed_batch < from_batch {
                tracing::info!(
                    from_batch,
                    last_committed_batch = l1_state.last_committed_batch,
                    "skipping L1Revert: already reverted or no batches to revert"
                );
                return Ok(false);
            }

            let on_chain_hash = fetch_batch(&l1_state.diamond_proxy_sl, from_batch, max_blocks)
                .await
                .context("failed to fetch on-chain hash for L1Revert from_batch")?
                .batch_info
                .hash();

            if on_chain_hash != from_batch_hash {
                tracing::info!(
                    from_batch,
                    ?on_chain_hash,
                    ?from_batch_hash,
                    "skipping L1Revert: from_batch_hash mismatch"
                );
                return Ok(false);
            }

            tracing::warn!(
                from_batch,
                last_l1_batch_to_keep,
                last_committed_batch = l1_state.last_committed_batch,
                "L1Revert: performing standalone L1 revert"
            );

            perform_l1_revert(
                last_l1_batch_to_keep,
                l1_state,
                chain_id,
                sl_provider,
                &l1_reverter_sk,
                persistent_batch_storage,
            )
            .await
            .context("failed to perform standalone startup L1 revert")?;

            Ok(true)
        }
    }
}
