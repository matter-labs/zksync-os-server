use alloy::primitives::U256;
use async_trait::async_trait;
use reth_revm::ExecuteCommitEvm;
use reth_revm::context::{Context, ContextTr};
use reth_revm::db::CacheDB;
use std::collections::HashSet;
use tokio::sync::mpsc::Sender;
use zksync_os_interface::types::BlockOutput;
use zksync_os_internal_config::InternalConfigManager;
use zksync_os_observability::{ComponentStateReporter, GenericComponentState};
use zksync_os_pipeline::{PeekableReceiver, PipelineComponent};
use zksync_os_revm::{DefaultZk, ZkBuilder};
use zksync_os_storage_api::{ReadStateHistory, ReplayRecord};
use zksync_os_types::ExecutionVersion;

use crate::helpers::{zk_spec_version, zk_tx_into_revm_tx};
use crate::revm_state_provider::RevmStateProvider;
use crate::storage_diff_comp::CompareReport;

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
        if self.revert_enabled && !report.is_empty() {
            let mut config = self.internal_config_manager.read_config()?;
            config.failing_block = Some(replay_record.block_context.block_number);

            let initial_blacklist_size = config.l2_signer_blacklist.len();
            for tx in &replay_record.transactions {
                config.l2_signer_blacklist.insert(tx.signer());
            }
            let new_blacklist_size = config.l2_signer_blacklist.len();
            tracing::info!(
                "Adding {} new addresses to L2 signer blacklist due to REVM inconsistency",
                new_blacklist_size - initial_blacklist_size
            );

            let message = format!(
                "REVM consistency check failed for block number {}, block hash {}",
                replay_record.block_context.block_number,
                block_output.header.hash(),
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
    type Input = (BlockOutput, ReplayRecord);
    type Output = (BlockOutput, ReplayRecord);

    const NAME: &'static str = "revm_consistency_checker";
    const OUTPUT_BUFFER_SIZE: usize = 5;

    async fn run(
        mut self,
        mut input: PeekableReceiver<Self::Input>, // PeekableReceiver<(BlockOutput, ReplayRecord)>
        output: Sender<Self::Output>,             // Sender<(BlockOutput, ReplayRecord)>
    ) -> anyhow::Result<()> {
        let latency_tracker = ComponentStateReporter::global().handle_for(
            "revm_consistency_checker",
            GenericComponentState::WaitingRecv,
        );
        // Remember unsupported execution versions to log only one warning for it.
        let mut warned_unsupported_versions: HashSet<u32> = HashSet::new();

        loop {
            latency_tracker.enter_state(GenericComponentState::WaitingRecv);
            let Some((block_output, replay_record)) = input.recv().await else {
                tracing::info!("inbound channel closed");
                return Ok(());
            };
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

            latency_tracker.enter_state(GenericComponentState::Processing);
            let state_block_number = replay_record.block_context.block_number - 1;
            let block_hashes = replay_record.block_context.block_hashes;
            let state_view = self
                .state
                .state_view_at(state_block_number)
                .map_err(anyhow::Error::from)?;

            if let Some(zk_spec) = zk_spec {
                // For each block, we create an in-memory cache database to accumulate transaction state changes separately
                let state_provider =
                    RevmStateProvider::new(state_view, block_hashes, state_block_number);
                let mut cache_db = CacheDB::new(state_provider);
                let mut evm = Context::default()
                    .with_db(&mut cache_db)
                    .modify_cfg_chained(|cfg| {
                        cfg.chain_id = replay_record.block_context.chain_id;
                        cfg.spec = zk_spec;
                    })
                    .modify_block_chained(|block| {
                        block.number = U256::from(replay_record.block_context.block_number);
                        block.timestamp = U256::from(replay_record.block_context.timestamp);
                        block.beneficiary = replay_record.block_context.coinbase;
                        block.basefee = replay_record.block_context.eip1559_basefee.saturating_to();
                        block.gas_limit = replay_record.block_context.gas_limit;
                        // `replay_record.block_context` holds an incorrect `prevrandao` value.
                        // We use the actual value that ZKsync OS uses instead.
                        block.prevrandao = Some(U256::ONE.into());
                    })
                    .build_zk();

                let revm_txs = replay_record
                    .transactions
                    .iter()
                    .zip(&block_output.tx_results)
                    .map(|(transaction, tx_output_raw)| {
                        let tx_output = tx_output_raw.as_ref().expect(
                            "block_output of a sealed block must not contain invalid transactions",
                        );

                        zk_tx_into_revm_tx(transaction, tx_output.gas_used, tx_output.is_success())
                    });

                // Commit after each tx
                for tx in revm_txs {
                    evm.transact_commit(tx)?;
                }

                let compare_report = CompareReport::build(
                    evm.0.db_mut(),
                    &block_output.storage_writes,
                    &block_output.account_diffs,
                )?;
                self.handle_report(&block_output, &replay_record, &compare_report)?;
            }

            latency_tracker.enter_state(GenericComponentState::WaitingSend);
            if output
                .send((block_output.clone(), replay_record.clone()))
                .await
                .is_err()
            {
                anyhow::bail!("Outbound channel closed");
            }
        }
    }
}
