use std::{convert::Infallible, str::FromStr};

use alloy::primitives::{Bytes, TxHash, B256};
use anyhow::Result;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};
use zksync_os_interface::types::BlockContext;
use zksync_os_storage_api::ReplayRecord;
use zksync_os_sequencer::model::blocks::BlockCommand;
use futures::StreamExt;
use zksync_os_types::{ZkTransaction};

use crate::replay_transport::replay_receiver;
use crate::two_fa::converter::zk_transaction_to_tx_env;
use revm::{
    context::{BlockEnv, Cfg, Context, Evm, JournalTr, LocalContext, Transaction, TxEnv}, database::{CacheDB, Database}, database_interface::EmptyDB, handler::EvmTr, inspector::InspectorEvmTr, primitives::Address, DatabaseCommit, ExecuteCommitEvm, InspectCommitEvm, MainBuilder, MainContext
};
use revm::context::ContextTr;
use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};
use zksync_revm::{evm::ZKsyncEvm, DefaultZk, ZkBuilder, ZkContext};

pub type SimpleEVM = Evm<Context<BlockEnv, TxEnv, revm::context::CfgEnv, CacheDB<reth_revm::db::EmptyDBTyped<std::convert::Infallible>>>, (), revm::handler::instructions::EthInstructions<revm::interpreter::interpreter::EthInterpreter, Context<BlockEnv, TxEnv, revm::context::CfgEnv, CacheDB<reth_revm::db::EmptyDBTyped<std::convert::Infallible>>>>, revm::handler::EthPrecompiles, revm::handler::EthFrame>;

#[derive(Debug, Clone)]
pub struct ValidationResult {
    pub success: bool,
    pub gas_used: u64,
    pub output: Bytes,
}

#[derive(Debug, Clone)]
pub struct TwoFaNodeConfig {
    pub main_node_address: String,
}

#[derive(Debug, Clone)]
pub struct StateDiffComparison {
    pub matches: bool,
    pub discrepancies: Vec<StateDiscrepancy>,
    pub gas_discrepancy: Option<GasDiscrepancy>,
}

#[derive(Debug, Clone)]
pub struct StateDiscrepancy {
    pub key: B256,
    pub main_node_value: B256,
    pub two_fa_value: B256,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct GasDiscrepancy {
    pub tx_hash: TxHash,
    pub main_node_gas: u64,
    pub two_fa_gas: u64,
    pub using_main_node_gas: bool,
}

#[derive(Debug, Clone)]
pub struct TransactionValidation {
    pub tx_hash: TxHash,
    pub success: bool,
    pub state_diff_comparison: StateDiffComparison,
    pub execution_time_ms: u64,
}

pub struct TwoFaNode {
    main_node_address: String,
    validation_results: mpsc::UnboundedSender<TransactionValidation>,
}

impl TwoFaNode {
    pub fn new(
        main_node_address: String,
        validation_results: mpsc::UnboundedSender<TransactionValidation>,
    ) -> Self {
        Self { main_node_address, validation_results }
    }

    pub async fn start(&self, mut db: CacheDB::<EmptyDB>) -> Result<()> 
    {
        info!("Starting 2FA node");
        info!("2FA node connecting to replay stream at {} from block 0",
              self.main_node_address);
        let mut stream = replay_receiver(0, &self.main_node_address)
            .await
            .map_err(|e| anyhow::anyhow!("replay_receiver failed: {e}"))?;

        while let Some(cmd) = stream.next().await {
            match cmd {
                BlockCommand::Replay(replay) => {
                    if let Err(e) = self.process_replay_record(&mut db, *replay).await {
                        error!("Failed to process replay record: {}", e);
                    }
                }
                other => {
                    debug!("Ignoring non-replay command: {}", other.to_string());
                }
            }
        }
        Ok(())
    }

    async fn process_replay_record(&self, db: &mut CacheDB::<EmptyDB>, replay_record: ReplayRecord) -> Result<()> 
    {
        let block_number = replay_record.block_context.block_number;
        info!("Processing block {} with {} transactions", block_number, replay_record.transactions.len());
        
        // Process transactions sequentially to maintain proper state changes
        let mut validation_results = Vec::new();
        for (tx_index, transaction) in replay_record.transactions.iter().enumerate() {
            let validation = Self::validate_transaction_simple(
                db,
                transaction.clone(), 
                tx_index, 
                &replay_record.block_context,
                &replay_record,
            ).await;
            validation_results.push(validation);
        }
        
        self.report_block_results(block_number, validation_results).await;
        Ok(())
    }

    async fn validate_transaction_simple(
        db: &mut CacheDB::<EmptyDB>,
        transaction: ZkTransaction, 
        tx_index: usize, 
        block_context: &BlockContext,
        replay_record: &ReplayRecord,
    ) -> TransactionValidation 
    {
        let start_time = std::time::Instant::now();
        let tx_hash = *transaction.hash();
        debug!("Validating transaction {} at index {}", tx_hash, tx_index);
        
        let execution_result = match Self::execute_with_validation(db, &transaction, block_context).await {
            Ok(result) => result,
            Err(e) => {
                error!("Failed to execute transaction {} with validation: {}", tx_hash, e);
                return TransactionValidation { 
                    tx_hash, 
                    success: false, 
                    state_diff_comparison: StateDiffComparison { 
                        matches: false, 
                        discrepancies: vec![], 
                        gas_discrepancy: None 
                    }, 
                    execution_time_ms: start_time.elapsed().as_millis() as u64 
                };
            }
        };
        
        let state_diff_comparison = Self::compare_execution_results(&execution_result, &transaction, tx_index, replay_record).await;
        let execution_time_ms = start_time.elapsed().as_millis() as u64;
        TransactionValidation { 
            tx_hash, 
            success: state_diff_comparison.matches, 
            state_diff_comparison, 
            execution_time_ms 
        }
    }

    async fn execute_with_validation(
        db: &mut CacheDB::<EmptyDB>,
        transaction: &ZkTransaction, 
        block_context: &BlockContext, 
    ) -> Result<ValidationResult> 
    {
        debug!("Executing transaction {} with reth EVM", transaction.hash());
        
        // Execute the transaction using simulate_tx
        let mut execution_context = block_context.clone();
        execution_context.eip1559_basefee = alloy::primitives::U256::from(0);
        
 
        let mut tracer = TracingInspector::new(
            TracingInspectorConfig::from_flat_call_config(&Default::default()),
        );

        let mut evm = Context::default()
            .with_db(db)
            .build_zk_with_inspector(&mut tracer);

        let try_tx = zk_transaction_to_tx_env(transaction);
        if let Ok((compatible_tx, to_mint, is_l1)) = try_tx {
            println!("lol: {}", compatible_tx.nonce());
            let account = (*evm.0.db_mut()).basic(compatible_tx.caller()).unwrap().unwrap_or_default();
            println!("balance_before={}, mint={}", account.balance, to_mint);
            if is_l1 {
                let account = (*evm.0.db_mut()).basic(compatible_tx.caller()).unwrap().unwrap_or_default();
                let account_debug = (*evm.0.db_mut()).basic(Address::from_str("0x000000000000000000000000000000000000800f").unwrap()).unwrap().unwrap_or_default();
                println!("nonce before: {}", account.nonce);
                println!("debug: {:?}", account_debug);
                (*evm.0.db_mut()).insert_account_info(compatible_tx.caller(), account);

            }
            let ref_tx = evm.inspect_tx_commit(compatible_tx);
            let trace_res = evm.0.inspector()
                .traces();
            println!("Trace: {:?}", trace_res);
        
            match ref_tx {
                Ok(tx_output) => {
                    println!(
                        "Transaction {} executed successfully: gas_used={}, is_result:={}", 
                        transaction.hash(), 
                        tx_output.gas_used(),
                        tx_output.is_success()
                    );
                    let account = (*evm.0.db_mut()).basic(transaction.signer()).unwrap().unwrap_or_default();
                    println!("balance after {}", account.balance);
                    println!("nonce after: {}", account.nonce);
                    Ok(ValidationResult {
                        success: true,
                        gas_used: tx_output.gas_used(),
                        output: Bytes::new(),
                    })
                }
                Err(invalid_tx) => {
                    warn!(
                        "Transaction {} execution failed: {:?}", 
                        transaction.hash(), 
                        invalid_tx
                    );
                    Ok(ValidationResult {
                        success: false,
                        gas_used: 0,
                        output: Bytes::new(),
                    })
                }
            }
        } else {
            // TODO: remove this, instead unwrap try_tx
            Ok(ValidationResult {
                success: false,
                gas_used: 0,
                output: Bytes::new(),
            })
        }
        
    }

    async fn compare_execution_results(
        validation_result: &ValidationResult, 
        zk_tx: &ZkTransaction, 
        tx_index: usize,
        replay_record: &ReplayRecord,
    ) -> StateDiffComparison {
        let discrepancies = Vec::new();
        let mut gas_discrepancy = None;
        
        // Extract the actual gas used from the main node's execution
        let main_node_gas = Self::extract_main_node_gas(zk_tx, tx_index, replay_record);
        let two_fa_gas = validation_result.gas_used;
        
        if main_node_gas != two_fa_gas {
            warn!(
                "Gas discrepancy for tx {}: main_node={}, 2fa={}", 
                zk_tx.hash(), 
                main_node_gas, 
                two_fa_gas
            );
            gas_discrepancy = Some(GasDiscrepancy { 
                tx_hash: *zk_tx.hash(), 
                main_node_gas, 
                two_fa_gas, 
                using_main_node_gas: false 
            });
        }
        
        let matches = discrepancies.is_empty() && gas_discrepancy.is_none();
        StateDiffComparison { matches, discrepancies, gas_discrepancy }
    }

    fn extract_main_node_gas(_zk_tx: &ZkTransaction, _tx_index: usize, _replay_record: &ReplayRecord) -> u64 {
        // TODO: In a full implementation, extract actual gas used from the main node's execution results
        // For now, we don't have access to transaction results in the replay record
        // This would require enhancing the replay record to include per-transaction results
        // or fetching them from the main node separately
        
        // Return 0 for now to indicate we don't have the real value
        // This means all transactions will show a gas discrepancy
        0
    }

    async fn report_block_results(&self, block_number: u64, validation_results: Vec<TransactionValidation>) {
        let total_txs = validation_results.len();
        let successful_validations = validation_results.iter().filter(|v| v.success).count();
        let failed_validations = total_txs - successful_validations;
        let avg_execution_time = if !validation_results.is_empty() { validation_results.iter().map(|v| v.execution_time_ms).sum::<u64>() / total_txs as u64 } else { 0 };
        info!("Block {} validation complete: {}/{} successful, avg execution time: {}ms", block_number, successful_validations, total_txs, avg_execution_time);
        for validation in validation_results { if let Err(e) = self.validation_results.send(validation) { error!("Failed to send validation result: {}", e); } }
        if failed_validations > 0 { warn!("Block {} had {} validation failures", block_number, failed_validations); }
    }
}
