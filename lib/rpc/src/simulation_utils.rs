use super::EthCallError;
use crate::eth_impl::{build_api_log, build_api_tx};
use crate::result::RevertError;
use alloy::consensus::Transaction as _;
use alloy::consensus::proofs::{calculate_receipt_root, calculate_transaction_root};
use alloy::network::primitives::BlockTransactions;
use alloy::primitives::{B256, Bloom, Bytes, U256};
use alloy::rpc::types::simulate::{SimCallResult, SimulateError};
use alloy::rpc::types::{BlockOverrides, TransactionRequest};
use zksync_os_interface::error::InvalidTransaction;
use zksync_os_interface::types::{
    BlockContext, BlockOutput, ExecutionOutput, ExecutionResult, TxOutput,
};
use zksync_os_rpc_api::types::ZkApiBlock;
use zksync_os_storage_api::state_override_view::OwnedOverrides;
use zksync_os_types::{ZkReceipt, ZkReceiptEnvelope, ZkTransaction};

#[derive(Debug)]
pub(super) struct SimulationStartContext {
    pub(super) block_context: BlockContext,
    pub(super) parent_block_number: u64,
    pub(super) parent_timestamp: u64,
}

pub(super) struct SimulatedBlockResponse {
    pub(super) inner: ZkApiBlock,
    pub(super) calls: Vec<SimCallResult>,
    pub(super) overlay: OwnedOverrides,
}

pub(super) fn build_simulated_block_response(
    block_context: BlockContext,
    txs: Vec<ZkTransaction>,
    block_output: BlockOutput,
    return_full_transactions: bool,
) -> Result<SimulatedBlockResponse, EthCallError> {
    let BlockOutput {
        header: sealed_header,
        tx_results,
        storage_writes,
        published_preimages,
        ..
    } = block_output;

    let mut block_bloom = Bloom::default();
    let mut number_of_logs_before_this_tx = 0;
    let mut cumulative_gas_used = 0;
    let mut receipts = Vec::with_capacity(tx_results.len());
    let mut simulated_txs = Vec::with_capacity(tx_results.len());
    let mut executed_tx_index = 0;

    for (call_index, (tx, result)) in txs.into_iter().zip(tx_results).enumerate() {
        let simulated_tx = match result {
            Ok(tx_output) => {
                let receipt = build_simulated_receipt(&tx, &tx_output, cumulative_gas_used);
                block_bloom.accrue_bloom(receipt.logs_bloom());
                cumulative_gas_used += tx_output.gas_used;
                let simulated_tx = SimulatedTx {
                    tx,
                    tx_index_in_block: executed_tx_index,
                    number_of_logs_before_this_tx,
                    result: SimulatedTxResult::Executed {
                        output: tx_output,
                        receipt: Box::new(receipt.clone()),
                    },
                };
                executed_tx_index += 1;
                number_of_logs_before_this_tx += receipt.logs().len() as u64;
                receipts.push(receipt);
                simulated_tx
            }
            Err(err) => SimulatedTx {
                tx,
                tx_index_in_block: call_index as u64,
                number_of_logs_before_this_tx,
                result: SimulatedTxResult::Invalid(err),
            },
        };
        simulated_txs.push(simulated_tx);
    }

    let mut header = sealed_header.unseal();
    header.base_fee_per_gas = Some(block_context.eip1559_basefee.saturating_to());
    header.logs_bloom = block_bloom;
    header.gas_used = cumulative_gas_used;
    let executed_envelopes = simulated_txs
        .iter()
        .filter(|tx| tx.is_executed())
        .map(|tx| tx.tx.envelope())
        .collect::<Vec<_>>();
    header.transactions_root = calculate_transaction_root(&executed_envelopes);
    header.receipts_root = calculate_receipt_root(&receipts);

    let header = alloy::rpc::types::Header::new(header);
    let block_hash = header.hash;
    let calls = simulated_txs
        .iter()
        .map(|tx| tx.to_call_result(block_hash, block_context))
        .collect();
    let transactions = if return_full_transactions {
        BlockTransactions::Full(
            simulated_txs
                .iter()
                .filter(|tx| tx.is_executed())
                .map(|tx| tx.to_api_tx(block_hash, block_context))
                .collect(),
        )
    } else {
        BlockTransactions::Hashes(
            simulated_txs
                .iter()
                .filter(|tx| tx.is_executed())
                .map(|tx| *tx.tx.hash())
                .collect(),
        )
    };
    let inner = ZkApiBlock::new(header, transactions);

    Ok(SimulatedBlockResponse {
        inner,
        calls,
        overlay: OwnedOverrides::new(
            storage_writes
                .into_iter()
                .map(|write| (write.key, write.value))
                .collect(),
            published_preimages.into_iter().collect(),
        ),
    })
}

fn build_simulated_receipt(
    tx: &ZkTransaction,
    tx_output: &TxOutput,
    cumulative_gas_used_before_this_tx: u64,
) -> ZkReceiptEnvelope {
    let l2_to_l1_logs = tx_output
        .l2_to_l1_logs
        .iter()
        .map(|l2_to_l1_log| l2_to_l1_log.log.clone().into())
        .collect();

    ZkReceiptEnvelope::from_typed(
        tx.tx_type(),
        ZkReceipt {
            status: matches!(tx_output.execution_result, ExecutionResult::Success(_)).into(),
            cumulative_gas_used: cumulative_gas_used_before_this_tx + tx_output.gas_used,
            logs: tx_output.logs.clone(),
            l2_to_l1_logs,
        },
    )
}

pub(super) fn next_block_context(
    mut block_context: BlockContext,
    parent_hash: B256,
) -> BlockContext {
    block_context.block_number += 1;
    block_context.timestamp += 1;
    block_context.block_hashes.0.rotate_left(1);
    block_context.block_hashes.0[255] = U256::from_be_bytes(parent_hash.0);
    block_context
}

pub(super) fn apply_simulate_block_overrides(
    block_context: &mut BlockContext,
    overrides: BlockOverrides,
    previous_block_number: u64,
    previous_timestamp: u64,
) -> Result<(), EthCallError> {
    if let Some(number) = overrides.number {
        let number = u64::try_from(number)
            .map_err(|_| EthCallError::SimulateInvalidBlockOverride("number"))?;
        if number <= previous_block_number {
            return Err(EthCallError::SimulateBlockNumberInvalid {
                got: number,
                parent: previous_block_number,
            });
        }
        let skipped_blocks = number.saturating_sub(block_context.block_number);
        if skipped_blocks >= 256 {
            block_context.block_hashes.0 = [U256::ZERO; 256];
        } else if skipped_blocks > 0 {
            let skipped_blocks = skipped_blocks as usize;
            block_context
                .block_hashes
                .0
                .copy_within(skipped_blocks.., 0);
            block_context.block_hashes.0[256 - skipped_blocks..].fill(U256::ZERO);
        }
        block_context.block_number = number;
    }
    if let Some(time) = overrides.time {
        if time <= previous_timestamp {
            return Err(EthCallError::SimulateBlockTimestampInvalid {
                got: time,
                parent: previous_timestamp,
            });
        }
        block_context.timestamp = time;
    }
    if let Some(gas_limit) = overrides.gas_limit {
        block_context.gas_limit = gas_limit;
    }
    if let Some(coinbase) = overrides.coinbase {
        block_context.coinbase = coinbase;
    }
    if let Some(random) = overrides.random {
        block_context.mix_hash = U256::from_be_bytes(random.0);
    } else {
        block_context.mix_hash = U256::ZERO;
    }
    if let Some(base_fee) = overrides.base_fee {
        block_context.eip1559_basefee = base_fee;
    }
    if let Some(blob_base_fee) = overrides.blob_base_fee {
        block_context.blob_fee = blob_base_fee;
    }
    // TODO: difficulty override is not propagated to BlockContext (ZKsync OS uses mix_hash
    // for prevrandao), so it is silently ignored.
    if let Some(block_hash_overrides) = overrides.block_hash {
        for (block_number, block_hash) in block_hash_overrides {
            if block_number >= block_context.block_number {
                continue;
            }
            let distance = block_context.block_number - block_number;
            if distance > 256 {
                continue;
            }

            let index = 256 - distance as usize;
            block_context.block_hashes.0[index] = U256::from_be_bytes(block_hash.0);
        }
    }

    Ok(())
}

pub(super) fn simulation_default_gas_limit(
    calls: &[TransactionRequest],
    block_gas_limit: u64,
    call_gas_limit: u64,
) -> Result<u64, EthCallError> {
    let total_specified_gas =
        calls
            .iter()
            .filter_map(|call| call.gas)
            .try_fold(0_u64, |sum, gas| {
                sum.checked_add(gas)
                    .ok_or(EthCallError::SimulateBlockGasLimitExceeded)
            })?;
    if total_specified_gas > block_gas_limit {
        return Err(EthCallError::SimulateBlockGasLimitExceeded);
    }

    let calls_without_gas = calls.iter().filter(|call| call.gas.is_none()).count() as u64;
    if calls_without_gas == 0 {
        return Ok(0);
    }

    let gas_per_call = (block_gas_limit - total_specified_gas) / calls_without_gas;
    Ok(if call_gas_limit == 0 {
        gas_per_call
    } else {
        gas_per_call.min(call_gas_limit)
    })
}

struct SimulatedTx {
    tx: ZkTransaction,
    tx_index_in_block: u64,
    number_of_logs_before_this_tx: u64,
    result: SimulatedTxResult,
}

enum SimulatedTxResult {
    Executed {
        output: TxOutput,
        receipt: Box<ZkReceiptEnvelope>,
    },
    Invalid(InvalidTransaction),
}

impl SimulatedTx {
    fn is_executed(&self) -> bool {
        matches!(&self.result, SimulatedTxResult::Executed { .. })
    }

    fn to_call_result(&self, block_hash: B256, block_context: BlockContext) -> SimCallResult {
        match &self.result {
            SimulatedTxResult::Executed { output, receipt } => {
                let logs = self.api_logs(block_hash, block_context, receipt);
                let (return_data, status, error) = match &output.execution_result {
                    ExecutionResult::Success(
                        ExecutionOutput::Call(return_bytes)
                        | ExecutionOutput::Create(return_bytes, _),
                    ) => (Bytes::from(return_bytes.clone()), true, None),
                    ExecutionResult::Revert(return_bytes) => {
                        let return_data = Bytes::from(return_bytes.clone());
                        (
                            return_data.clone(),
                            false,
                            Some(SimulateError {
                                code: -32000,
                                message: RevertError::new(return_data).to_string(),
                            }),
                        )
                    }
                };

                SimCallResult {
                    return_data,
                    logs,
                    gas_used: output.gas_used,
                    status,
                    error,
                }
            }
            SimulatedTxResult::Invalid(err) => SimCallResult {
                return_data: Bytes::default(),
                logs: vec![],
                gas_used: 0,
                status: false,
                error: Some(SimulateError {
                    code: -32015,
                    message: format!("vm execution error: {err}"),
                }),
            },
        }
    }

    fn to_api_tx(
        &self,
        block_hash: B256,
        block_context: BlockContext,
    ) -> zksync_os_rpc_api::types::ZkApiTransaction {
        build_api_tx(
            self.tx.clone(),
            Some(&self.tx_meta(block_hash, block_context, self.gas_used())),
        )
    }

    fn api_logs(
        &self,
        block_hash: B256,
        block_context: BlockContext,
        receipt: &ZkReceiptEnvelope,
    ) -> Vec<alloy::rpc::types::Log> {
        let tx_hash = *self.tx.hash();
        let meta = self.tx_meta(block_hash, block_context, self.gas_used());
        receipt
            .logs()
            .iter()
            .cloned()
            .enumerate()
            .map(|(i, log)| build_api_log(tx_hash, log, meta.clone(), i as u64))
            .collect()
    }

    fn tx_meta(
        &self,
        block_hash: B256,
        block_context: BlockContext,
        gas_used: u64,
    ) -> zksync_os_storage_api::TxMeta {
        zksync_os_storage_api::TxMeta {
            block_hash,
            block_number: block_context.block_number,
            block_timestamp: block_context.timestamp,
            tx_index_in_block: self.tx_index_in_block,
            effective_gas_price: self
                .tx
                .inner
                .inner()
                .effective_gas_price(Some(block_context.eip1559_basefee.saturating_to())),
            number_of_logs_before_this_tx: self.number_of_logs_before_this_tx,
            gas_used,
            contract_address: match &self.result {
                SimulatedTxResult::Executed { output, .. } => output.contract_address,
                SimulatedTxResult::Invalid(_) => None,
            },
        }
    }

    fn gas_used(&self) -> u64 {
        match &self.result {
            SimulatedTxResult::Executed { output, .. } => output.gas_used,
            SimulatedTxResult::Invalid(_) => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zksync_os_interface::types::BlockHashes;

    #[test]
    fn simulation_default_gas_limit_splits_remaining_block_gas() {
        let calls = vec![
            TransactionRequest {
                gas: Some(40),
                ..Default::default()
            },
            TransactionRequest::default(),
            TransactionRequest::default(),
        ];

        assert_eq!(simulation_default_gas_limit(&calls, 100, 0).unwrap(), 30);
        assert_eq!(simulation_default_gas_limit(&calls, 100, 25).unwrap(), 25);
    }

    #[test]
    fn simulation_default_gas_limit_rejects_block_gas_overflow() {
        let calls = vec![TransactionRequest {
            gas: Some(101),
            ..Default::default()
        }];

        assert!(matches!(
            simulation_default_gas_limit(&calls, 100, 0),
            Err(EthCallError::SimulateBlockGasLimitExceeded)
        ));
    }

    #[test]
    fn simulate_block_overrides_reject_non_increasing_sequences() {
        let mut context = BlockContext {
            block_number: 11,
            timestamp: 101,
            ..Default::default()
        };
        let number_err = apply_simulate_block_overrides(
            &mut context,
            BlockOverrides {
                number: Some(U256::from(10)),
                ..Default::default()
            },
            10,
            100,
        )
        .unwrap_err();
        assert!(matches!(
            number_err,
            EthCallError::SimulateBlockNumberInvalid { .. }
        ));

        let time_err = apply_simulate_block_overrides(
            &mut context,
            BlockOverrides {
                time: Some(100),
                ..Default::default()
            },
            10,
            100,
        )
        .unwrap_err();
        assert!(matches!(
            time_err,
            EthCallError::SimulateBlockTimestampInvalid { .. }
        ));
    }

    #[test]
    fn simulate_block_override_number_jump_clears_gap_hashes() {
        let mut hashes = [U256::ZERO; 256];
        for (i, hash) in hashes.iter_mut().enumerate() {
            *hash = U256::from(i + 1);
        }
        let mut context = BlockContext {
            block_number: 11,
            timestamp: 101,
            block_hashes: BlockHashes(hashes),
            ..Default::default()
        };

        apply_simulate_block_overrides(
            &mut context,
            BlockOverrides {
                number: Some(U256::from(14)),
                ..Default::default()
            },
            10,
            100,
        )
        .unwrap();

        assert_eq!(context.block_number, 14);
        assert_eq!(context.block_hashes.0[252], U256::from(256));
        assert_eq!(context.block_hashes.0[253], U256::ZERO);
        assert_eq!(context.block_hashes.0[254], U256::ZERO);
        assert_eq!(context.block_hashes.0[255], U256::ZERO);
    }
}
