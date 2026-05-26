use crate::config::Config;
use alloy::network::{Ethereum, EthereumWallet};
use alloy::providers::{Provider, WalletProvider};
use anyhow::Context as _;
use ruint::aliases::U256;
use zksync_os_contract_interface::IValidatorTimelock;
use zksync_os_contract_interface::l1_discovery::L1State;
use zksync_os_l1_watcher::util;
use zksync_os_storage::db::ExecutedBatchStorage;
use zksync_os_storage_api::{PersistedBatch, ReadBatch};

/// Derives the `from_block` value for `sequencer.block_rebuild` based on `sequencer.l1_revert`.
///
/// Must be called **before** the revert transaction lands on L1, because the L1 fallback path
/// relies on `totalBatchesCommitted` still being >= the reverted batch at the current L1 head.
pub async fn derive_block_rebuild_from_block(
    config: &Config,
    l1_state: &L1State,
    persistent_batch_storage: &ExecutedBatchStorage,
) -> anyhow::Result<u64> {
    let l1_revert = config
        .sequencer_config
        .l1_revert
        .as_ref()
        .expect("l1_revert must be configured before deriving block rebuild from_block");

    let reverted_batch = l1_revert
        .last_l1_batch_to_keep
        .checked_add(1)
        .expect("`sequencer.l1_revert.last_l1_batch_to_keep` overflowed");
    // Block 0 is genesis and is never executed, so the earliest rebuild point is block 1.
    // For batch 1 there is no predecessor in storage to look up its first block from.
    let derived_from_block = if reverted_batch == 1 {
        1
    } else {
        // Fast path: the batch watcher already persisted this batch locally.
        let local_batch: Option<PersistedBatch> =
            persistent_batch_storage.get_batch_by_number(reverted_batch)?;
        if let Some(batch) = local_batch {
            batch.first_block_number()
        } else {
            // Fallback: local storage doesn't have the batch. Binary-search L1 for the commit
            // block and decode the `ReportCommittedBatchRangeZKsyncOS` event to get firstBlockNumber.
            tracing::info!(
                reverted_batch,
                last_l1_batch_to_keep = l1_revert.last_l1_batch_to_keep,
                "batch not found in local storage, querying L1 for block range"
            );
            let sl_block_with_commit = util::find_l1_commit_block_by_batch_number(
                l1_state.diamond_proxy_sl.clone(),
                reverted_batch,
                config.l1_watcher_config.max_blocks_to_process,
            )
            .await
            .with_context(|| {
                format!(
                    "failed to find L1 commit block for batch {reverted_batch} \
                     while deriving `sequencer.block_rebuild.from_block`"
                )
            })?;
            util::fetch_stored_batch_data(
                &l1_state.diamond_proxy_sl,
                sl_block_with_commit,
                reverted_batch,
            )
            .await
            .with_context(|| {
                format!(
                    "failed to fetch batch {reverted_batch} data from L1 \
                     while deriving `sequencer.block_rebuild.from_block`"
                )
            })?
            .with_context(|| {
                format!(
                    "cannot derive `sequencer.block_rebuild.from_block` for \
                     `sequencer.l1_revert.last_l1_batch_to_keep={}`: \
                     batch {} was not found in local storage or on L1",
                    l1_revert.last_l1_batch_to_keep, reverted_batch
                )
            })?
            .first_block_number()
        }
    };

    Ok(derived_from_block)
}

pub async fn perform_l1_revert<T>(
    config: &Config,
    l1_state: &L1State,
    chain_id: u64,
    l1_provider: &T,
    gateway_provider: &Option<T>,
) -> anyhow::Result<()>
where
    T: Provider<Ethereum> + WalletProvider<Wallet = EthereumWallet> + Clone + 'static,
{
    let l1_revert = config
        .sequencer_config
        .l1_revert
        .as_ref()
        .expect("l1_revert must be configured before startup L1 revert");
    let reverter = config
        .l1_sender_config
        .reverter_sk
        .as_ref()
        .unwrap_or_else(|| {
            panic!("`l1_sender.reverter_sk` must be set when `sequencer.l1_revert` is configured")
        });

    let last_l1_batch_to_keep = l1_revert.last_l1_batch_to_keep;

    let mut sl_provider = if l1_state.l1_chain_id == l1_state.sl_chain_id {
        l1_provider.clone()
    } else {
        gateway_provider.clone().unwrap_or_else(|| {
            panic!(
                "startup L1 revert requires `gateway_provider` because the chain settles on Gateway"
            )
        })
    };

    let reverter_address = reverter
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
        target_batch = last_l1_batch_to_keep,
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
        target_batch = last_l1_batch_to_keep,
        "startup L1 revert completed"
    );

    Ok(())
}
