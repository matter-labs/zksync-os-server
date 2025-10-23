use alloy::primitives::U256;
use async_trait::async_trait;
use reth_revm::db::CacheDB;

use reth_revm::ExecuteCommitEvm;
use reth_revm::context::{Context, ContextTr};
use tokio::sync::mpsc::Sender;
use zksync_os_interface::types::BlockOutput;
use zksync_os_observability::{ComponentStateReporter, GenericComponentState};
use zksync_os_pipeline::{PeekableReceiver, PipelineComponent};
use zksync_os_revm::{DefaultZk, ZkBuilder};
use zksync_os_storage_api::{ReadStateHistory, ReplayRecord};

use crate::helpers::zk_tx_into_revm_tx;
use crate::revm_state_provider::RevmStateProvider;
use crate::storage_diff_comp::CompareReport;

pub struct RevmConsistencyChecker<State>
where
    State: ReadStateHistory + Clone + Send + 'static,
{
    state: State,
}

impl<State> RevmConsistencyChecker<State>
where
    State: ReadStateHistory + Clone + Send + 'static,
{
    pub fn new(state: State) -> Self {
        Self { state }
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
        loop {
            latency_tracker.enter_state(GenericComponentState::WaitingRecv);
            let Some((block_output, replay_record)) = input.recv().await else {
                anyhow::bail!("inbound channel closed");
            };

            latency_tracker.enter_state(GenericComponentState::Processing);
            let state_block_number = replay_record.block_context.block_number - 1;
            let block_hashes = replay_record.block_context.block_hashes;
            let state_view = self
                .state
                .state_view_at(state_block_number)
                .map_err(anyhow::Error::from)?;

            {
                // For each block, we create an in-memory cache database to accumulate transaction state changes separately
                let state_provider =
                    RevmStateProvider::new(state_view, block_hashes, state_block_number);
                let mut cache_db = CacheDB::new(state_provider);
                let mut evm = Context::default()
                    .with_db(&mut cache_db)
                    .modify_cfg_chained(|cfg| {
                        cfg.chain_id = replay_record.block_context.chain_id;
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
                    .filter_map(|(transaction, tx_output_raw)| {
                        let tx_output = match tx_output_raw {
                            Ok(tx_output) => tx_output,
                            _ => return None, // Skip invalid transaction as they are not included in the batch
                        };

                        Some(zk_tx_into_revm_tx(
                            transaction,
                            tx_output.gas_used,
                            tx_output.is_success(),
                        ))
                    });

                evm.transact_many_commit(revm_txs)?;
                let compare_report = CompareReport::build(
                    evm.0.db_mut(),
                    &block_output.storage_writes,
                    &block_output.account_diffs,
                )?;
                compare_report.log_tracing(20);
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
