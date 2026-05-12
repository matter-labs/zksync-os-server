use alloy::primitives::{B256, U256};
use async_trait::async_trait;
use revm::ExecuteCommitEvm;
use revm::context::ContextTr;
use revm::context_interface::block::BlobExcessGasAndPrice;
use revm::database::{CacheDB, EmptyDB};
use ruint::aliases::B160;
use std::collections::HashSet;
use tokio::sync::mpsc;
use zk_ee::common_structs::derive_flat_storage_key;
use zk_ee::utils::Bytes32;
use zksync_os_interface::types::BlockOutput;
use zksync_os_internal_config::InternalConfigManager;
use zksync_os_observability::{ComponentStateReporter, GenericComponentState};
use zksync_os_pipeline::{PeekableReceiver, PipelineComponent, SendAndRecordExt};
use zksync_os_revm::{DefaultZk, ZkBuilder, ZkContext, ZkSpecId};
use zksync_os_sequencer::model::blocks::AppliedBlock;
use zksync_os_storage_api::{ReadStateHistory, ReplayRecord, ViewState};
use zksync_os_types::{ExecutionVersion, SYSTEM_CONTEXT_ADDRESS};

use crate::helpers::{zk_spec_version, zk_tx_into_revm_tx};
use crate::metrics::PUSH_METRICS;
use crate::revm_state_provider::RevmStateProvider;
use crate::storage_diff_comp::CompareReport;

const BLOB_BASE_FEE_UPDATE_FRACTION: u128 = alloy::eips::eip4844::BLOB_GASPRICE_UPDATE_FRACTION;
const MIN_BASE_FEE_PER_BLOB_GAS: u128 = alloy::eips::eip4844::BLOB_TX_MIN_BLOB_GASPRICE;

pub struct RevmConsistencyChecker<State>
where
    State: ReadStateHistory + Clone + Send + 'static,
{
    state: State,
    internal_config_manager: InternalConfigManager,
    revert_enabled: bool,
}

impl<State> RevmConsistencyChecker<State>
where
    State: ReadStateHistory + Clone + Send + 'static,
{
    pub fn new(
        state: State,
        internal_config_manager: InternalConfigManager,
        revert_enabled: bool,
    ) -> Self {
        Self {
            state,
            internal_config_manager,
            revert_enabled,
        }
    }

    pub fn handle_report(
        &self,
        block_output: &BlockOutput,
        replay_record: &ReplayRecord,
        report: &CompareReport,
    ) -> anyhow::Result<()> {
        report.log_tracing(20);
        if report.is_empty() {
            return Ok(());
        }

        let message = format!(
            "REVM consistency check failed for block number {}, block hash {}",
            replay_record.block_context.block_number,
            block_output.header.hash(),
        );
        tracing::warn!(message);

        // Update metric for the divergence alert
        PUSH_METRICS.revm_divergences_detected.inc();

        if self.revert_enabled {
            let mut config = self.internal_config_manager.read_config()?;
            config.failing_block = Some(replay_record.block_context.block_number);

            let initial_blacklist_size = config.l2_signer_blacklist.len();
            for tx in &replay_record.transactions {
                config.l2_signer_blacklist.insert(tx.signer());
            }
            let new_blacklist_size = config.l2_signer_blacklist.len();
            tracing::info!(
                "Adding {} new addresses to L2 signer blacklist due to REVM divergence",
                new_blacklist_size - initial_blacklist_size
            );

            self.internal_config_manager
                .write_config_and_panic(&config, &message)?;
        }

        Ok(())
    }
}

#[async_trait]
impl<State> PipelineComponent for RevmConsistencyChecker<State>
where
    State: ReadStateHistory + Clone + Send + 'static,
{
    type Input = AppliedBlock;
    type Output = AppliedBlock;

    const COMPONENT_ID: zksync_os_pipeline::ComponentId =
        zksync_os_pipeline::ComponentId::RevmConsistencyChecker;

    async fn run(
        self,
        mut input: PeekableReceiver<Self::Input>,
        output: mpsc::Sender<Self::Output>,
        state_reporter: ComponentStateReporter,
    ) -> anyhow::Result<()> {
        // Remember unsupported execution versions to log only one warning for it.
        let mut warned_unsupported_versions: HashSet<u32> = HashSet::new();

        loop {
            state_reporter.enter_state(GenericComponentState::Idle);
            let Some(AppliedBlock {
                output: block,
                record: replay_record,
            }) = input.recv_and_record_picked(&state_reporter).await
            else {
                tracing::info!("inbound channel closed");
                return Ok(());
            };
            let block_output = block.as_ref();

            let raw_exec_ver = replay_record.block_context.execution_version;
            let zk_spec = match ExecutionVersion::try_from(raw_exec_ver)
                .ok()
                .and_then(zk_spec_version)
            {
                Some(spec) => Some(spec),
                None => {
                    // Warn once per execution_version. Afterwards log at info level.
                    let first_time = warned_unsupported_versions.insert(raw_exec_ver);
                    if first_time {
                        tracing::warn!(
                            execution_version = raw_exec_ver,
                            "Invalid or unsupported ZKsync OS execution version for REVM; skipping block"
                        );
                    } else {
                        tracing::info!(
                            execution_version = raw_exec_ver,
                            "Invalid or unsupported ZKsync OS execution version for REVM; skipping block"
                        );
                    }
                    // Skip executing this block when there is no supported REVM version.
                    None
                }
            };

            state_reporter.enter_state(GenericComponentState::Active);
            let state_block_number = replay_record.block_context.block_number - 1;
            let block_hashes = replay_record.block_context.block_hashes;
            let mut state_view = self
                .state
                .state_view_at(state_block_number)
                .map_err(anyhow::Error::from)?;

            if let Some(zk_spec) = zk_spec {
                let settlement_layer_chain_id = read_settlement_layer_chain_id(&mut state_view);

                // Saturating: extreme fees are unrealistic; clamping keeps the
                // checker running rather than tearing down the pipeline.
                let block_basefee: u64 =
                    replay_record.block_context.eip1559_basefee.saturating_to();

                // AtlasV1/V2 didn't honor `block_context.mix_hash` for prevrandao (ZKsync OS
                // hardcoded `1`) and didn't surface blob fees. Both fields only became
                // meaningful with AtlasV3 — gating them keeps the historical check accurate.
                //
                // The pre-AtlasV3 `blob_excess_gas_and_price` must still be `Some`: all Atlas
                // specs map to Cancun and revm's header validation rejects a missing value.
                // Use the same value `BlockEnv::default()` would have produced, since that's
                // what the pre-PR checker effectively passed.
                let (prevrandao, blob_excess_gas_and_price) =
                    if ZkSpecId::AtlasV3.is_enabled_in(zk_spec) {
                        let blob_fee: u64 = replay_record.block_context.blob_fee.saturating_to();
                        let blob_excess_gas = calculate_excess_blob_gas_from_blob_base_fee(
                            blob_fee,
                            BLOB_BASE_FEE_UPDATE_FRACTION,
                        );
                        let blob_excess = BlobExcessGasAndPrice::new(
                            blob_excess_gas,
                            BLOB_BASE_FEE_UPDATE_FRACTION
                                .try_into()
                                .expect("Blob base fee update fraction should fit into u64"),
                        );
                        (
                            replay_record.block_context.mix_hash.into(),
                            Some(blob_excess),
                        )
                    } else {
                        let pre_v3_blob_default = BlobExcessGasAndPrice::new(
                            0,
                            revm::primitives::eip4844::BLOB_BASE_FEE_UPDATE_FRACTION_PRAGUE,
                        );
                        (B256::from(U256::ONE), Some(pre_v3_blob_default))
                    };

                // For each block, we create an in-memory cache database to accumulate transaction state changes separately
                let state_provider =
                    RevmStateProvider::new(state_view, block_hashes, state_block_number);
                let cache_db = CacheDB::new(state_provider);
                let mut evm = ZkContext::<EmptyDB>::default()
                    .with_db(cache_db)
                    .modify_cfg_chained(|cfg| {
                        cfg.chain_id = replay_record.block_context.chain_id;
                        cfg.spec = zk_spec;
                    })
                    .modify_block_chained(|block| {
                        block.number = U256::from(replay_record.block_context.block_number);
                        block.timestamp = U256::from(replay_record.block_context.timestamp);
                        block.beneficiary = replay_record.block_context.coinbase;
                        block.basefee = block_basefee;
                        block.gas_limit = replay_record.block_context.gas_limit;
                        block.prevrandao = Some(prevrandao);
                        block.blob_excess_gas_and_price = blob_excess_gas_and_price;
                    })
                    .build_zk();

                let revm_txs: anyhow::Result<Vec<_>> = replay_record
                    .transactions
                    .iter()
                    .zip(&block_output.tx_results)
                    .map(|(transaction, tx_output_raw)| {
                        let tx_output = tx_output_raw.as_ref().expect(
                            "block_output of a sealed block must not contain invalid transactions",
                        );
                        zk_tx_into_revm_tx(
                            transaction,
                            tx_output.gas_used,
                            tx_output.is_success(),
                            replay_record.block_context.gas_limit,
                            Some(settlement_layer_chain_id),
                        )
                    })
                    .collect();

                match revm_txs {
                    Ok(txs) => {
                        // Commit after each tx
                        for tx in txs {
                            evm.transact_commit(tx)?;
                        }

                        let compare_report = CompareReport::build(
                            evm.0.db_mut(),
                            &block_output.storage_writes,
                            &block_output.account_diffs,
                        )?;
                        self.handle_report(block_output, &replay_record, &compare_report)?;
                    }
                    Err(err) => {
                        // Tx conversion failed (e.g. malformed envelope) — skip
                        // the whole block rather than blocking the pipeline.
                        PUSH_METRICS.revm_blocks_skipped.inc();
                        tracing::warn!(
                            block_number = replay_record.block_context.block_number,
                            "Skipping REVM consistency check for block: {err:#}"
                        );
                    }
                }
            }

            output.send_and_record(
                AppliedBlock {
                    output: block,
                    record: replay_record,
                },
                &state_reporter,
            )?;
        }
    }
}

/// Read the settlement layer chain id from `SYSTEM_CONTEXT_ADDRESS`, slot 0.
fn read_settlement_layer_chain_id<S: ViewState>(state: &mut S) -> U256 {
    let address = B160::from_be_bytes(SYSTEM_CONTEXT_ADDRESS.into_array());
    let flat_key = derive_flat_storage_key(&address, &Bytes32::ZERO);
    let value = state
        .read(B256::from(flat_key.as_u8_array()))
        .unwrap_or_default();
    U256::from_be_slice(value.as_slice())
}

/// Inverse of `fake_exponential` over the blob base fee, used to derive
/// `excess_blob_gas` from a target blob base fee.
fn calculate_excess_blob_gas_from_blob_base_fee(
    blob_base_fee: u64,
    blob_base_fee_update_fraction: u128,
) -> u64 {
    if (blob_base_fee as u128) <= MIN_BASE_FEE_PER_BLOB_GAS {
        return 0;
    }
    assert!(
        blob_base_fee_update_fraction != 0,
        "blob base fee update fraction cannot be zero"
    );

    let target_blob_base_fee = blob_base_fee as u128;
    let mut low = 0u64;
    let mut high = 1u64;

    while calculate_blob_base_fee_for_excess_blob_gas(high, blob_base_fee_update_fraction)
        < target_blob_base_fee
    {
        if high == u64::MAX {
            return u64::MAX;
        }
        high = high.saturating_mul(2);
    }

    while low < high {
        let mid = low + (high - low) / 2;
        let blob_base_fee_at_mid =
            calculate_blob_base_fee_for_excess_blob_gas(mid, blob_base_fee_update_fraction);
        if blob_base_fee_at_mid < target_blob_base_fee {
            low = mid + 1;
        } else {
            high = mid;
        }
    }

    low
}

fn calculate_blob_base_fee_for_excess_blob_gas(
    excess_blob_gas: u64,
    blob_base_fee_update_fraction: u128,
) -> u128 {
    alloy::eips::eip4844::fake_exponential(
        alloy::eips::eip4844::BLOB_TX_MIN_BLOB_GASPRICE,
        excess_blob_gas as u128,
        blob_base_fee_update_fraction,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_blob_base_fee_maps_to_zero_excess_blob_gas() {
        assert_eq!(
            calculate_excess_blob_gas_from_blob_base_fee(0, BLOB_BASE_FEE_UPDATE_FRACTION),
            0
        );
    }

    #[test]
    fn excess_blob_gas_inverse_returns_minimum_matching_value() {
        let test_cases = [0u64, 1, 2, 100_000, 2_314_058, 10_000_000];
        for excess_blob_gas in test_cases {
            let blob_base_fee = calculate_blob_base_fee_for_excess_blob_gas(
                excess_blob_gas,
                BLOB_BASE_FEE_UPDATE_FRACTION,
            );
            let blob_base_fee_u64: u64 = blob_base_fee
                .try_into()
                .expect("test vector should fit into u64");

            let recovered_excess_blob_gas = calculate_excess_blob_gas_from_blob_base_fee(
                blob_base_fee_u64,
                BLOB_BASE_FEE_UPDATE_FRACTION,
            );

            // Inverse must round up (not down): the recovered value re-evaluates
            // to a fee >= the original target...
            let recovered_blob_base_fee = calculate_blob_base_fee_for_excess_blob_gas(
                recovered_excess_blob_gas,
                BLOB_BASE_FEE_UPDATE_FRACTION,
            );
            assert!(recovered_blob_base_fee >= blob_base_fee);

            // ...and the value just below recovers to a strictly smaller fee,
            // confirming it's the minimum match.
            if recovered_excess_blob_gas > 0 {
                let previous_blob_base_fee = calculate_blob_base_fee_for_excess_blob_gas(
                    recovered_excess_blob_gas - 1,
                    BLOB_BASE_FEE_UPDATE_FRACTION,
                );
                assert!(previous_blob_base_fee < blob_base_fee);
            }
        }
    }
}
