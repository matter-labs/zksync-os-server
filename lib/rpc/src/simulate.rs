use crate::eth_call_handler::{EthCallError, EthCallHandler};
use crate::eth_impl::{build_api_log, build_api_tx};
use crate::result::RevertError;
use crate::rpc_storage::{ReadRpcStorage, RpcStorageError};
use alloy::consensus::Transaction as _;
use alloy::consensus::proofs::{calculate_receipt_root, calculate_transaction_root};
use alloy::eips::BlockId;
use alloy::network::primitives::BlockTransactions;
use alloy::primitives::{B256, Bloom, Bytes, U256};
use alloy::rpc::types::simulate::{
    MAX_SIMULATE_BLOCKS, SimCallResult, SimulateError, SimulatePayload, SimulatedBlock,
};
use alloy::rpc::types::{BlockOverrides, TransactionRequest};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use zk_os_api::helpers::get_nonce;
use zksync_os_interface::error::InvalidTransaction;
use zksync_os_interface::tracing::{NopTracer, NopValidator};
use zksync_os_interface::traits::{NoopTxCallback, TxListSource};
use zksync_os_interface::types::{
    BlockContext, BlockOutput, ExecutionOutput, ExecutionResult, TxOutput,
};
use zksync_os_multivm::run_block;
use zksync_os_rpc_api::types::ZkApiBlock;
use zksync_os_storage_api::ViewState;
use zksync_os_storage_api::state_override_view::{
    OverriddenStateView, OwnedOverrides, build_state_override_maps,
};
use zksync_os_types::{ZkReceipt, ZkReceiptEnvelope, ZkTransaction, ZksyncOsEncode};

impl<RpcStorage: ReadRpcStorage> EthCallHandler<RpcStorage> {
    /// Implements `eth_simulateV1` using the same high-level model as reth: execute the requested
    /// blocks linearly, carry an overlay of simulated writes into subsequent blocks, and use a
    /// separate execution context when `validation=false` so fee validation does not leak into the
    /// returned header.
    ///
    /// # Spec limitations
    ///
    /// The following features from the `eth_simulateV1` spec are not supported:
    ///
    /// - `traceTransfers=true`: rejected with an error. ZKsync OS has no transfer-tracing
    ///   inspector equivalent, so synthetic ERC-20 transfer logs cannot be generated.
    /// - `movePrecompileToAddress`: rejected with an error. The VM does not support remapping
    ///   precompile addresses on a per-block basis.
    /// - `blockOverrides.difficulty`: silently ignored. ZKsync OS has no `difficulty` field in
    ///   its block context; use `blockOverrides.random` (prevrandao) instead.
    /// - `blockOverrides.parentBeaconBlockRoot`: silently ignored. There is no corresponding
    ///   field in `BlockContext`.
    /// - `validation=false` nonce relaxation: partially unsupported. The basefee is zeroed as
    ///   the spec requires, but nonce checks are not disabled. Transactions without an explicit
    ///   nonce are auto-filled from state (the common case works), but an explicitly supplied
    ///   stale nonce will be rejected by the VM.
    pub fn simulate_v1_impl(
        &self,
        opts: SimulatePayload,
        block: Option<BlockId>,
    ) -> Result<Vec<SimulatedBlock<ZkApiBlock>>, EthCallError> {
        let SimulatePayload {
            block_state_calls,
            trace_transfers,
            validation,
            return_full_transactions,
        } = opts;

        if block_state_calls.is_empty() {
            return Err(EthCallError::SimulateInvalidParams(
                "calls are empty".to_string(),
            ));
        }
        if block_state_calls.len() > MAX_SIMULATE_BLOCKS as usize {
            return Err(EthCallError::SimulateInvalidParams(
                "too many blocks".to_string(),
            ));
        }

        if trace_transfers {
            return Err(EthCallError::SimulateInvalidParams(
                "traceTransfers is not implemented".to_string(),
            ));
        }

        let SimulationStartContext {
            mut block_context,
            parent_block_number,
            parent_timestamp,
        } = self.resolve_simulation_start_context(block)?;
        let base_state = self.storage.state_view_at(parent_block_number)?;
        let mut overlays = Arc::new(BTreeMap::new());
        let mut simulated_blocks = Vec::with_capacity(block_state_calls.len());
        let mut previous_block_number = parent_block_number;
        let mut previous_timestamp = parent_timestamp;

        for sim_block in block_state_calls {
            // Per the eth_simulateV1 spec, prevrandao defaults to zero for simulated blocks
            // unless explicitly overridden via `blockOverrides.random`.
            block_context.mix_hash = U256::ZERO;
            if let Some(block_overrides) = sim_block.block_overrides {
                apply_simulate_block_overrides(
                    &mut block_context,
                    block_overrides,
                    previous_block_number,
                    previous_timestamp,
                    self.config.eth_simulate_block_gas_limit,
                )?;
            }

            let response_context = block_context;
            let mut execution_context = response_context;
            if !validation {
                execution_context.eip1559_basefee = U256::ZERO;
            }

            let simulation_view =
                OverriddenStateView::new(base_state.clone(), Arc::clone(&overlays));

            let state_overrides = match sim_block.state_overrides {
                Some(state_overrides) => {
                    if state_overrides
                        .values()
                        .any(|account| account.move_precompile_to.is_some())
                    {
                        return Err(EthCallError::SimulateMovePrecompileNotSupported);
                    }
                    build_state_override_maps(&simulation_view, state_overrides)
                }
                None => OwnedOverrides::default(),
            };
            let overridden_view =
                OverriddenStateView::new(simulation_view, state_overrides.clone());
            let txs = self.create_simulation_txs(
                sim_block.calls,
                execution_context,
                overridden_view.clone(),
            )?;
            let tx_source = TxListSource {
                transactions: txs.iter().cloned().map(|tx| tx.encode()).collect(),
            };
            let block_output = run_block(
                execution_context,
                overridden_view.clone(),
                overridden_view,
                tx_source,
                NoopTxCallback,
                &mut NopTracer,
                &mut NopValidator,
            )
            .map_err(EthCallError::ForwardSubsystemError)?;

            let (simulated_block, block_overlay) = build_simulated_block_response(
                response_context,
                txs,
                block_output,
                return_full_transactions,
            )?;
            let next_hash = simulated_block.inner.header.hash;

            let mut overlay = state_overrides;
            // State overrides seed this simulated block; actual VM writes take precedence for
            // subsequent simulated blocks.
            overlay.extend(block_overlay);
            Arc::get_mut(&mut overlays)
                .ok_or_else(|| {
                    EthCallError::ForwardSubsystemError(anyhow::anyhow!(
                        "simulation overlay still borrowed during mutation"
                    ))
                })?
                .insert(block_context.block_number, overlay);
            previous_block_number = block_context.block_number;
            previous_timestamp = block_context.timestamp;
            simulated_blocks.push(simulated_block);

            block_context = next_block_context(block_context, next_hash);
        }

        Ok(simulated_blocks)
    }

    fn resolve_simulation_start_context(
        &self,
        block: Option<BlockId>,
    ) -> Result<SimulationStartContext, EthCallError> {
        let block = block.unwrap_or_default();
        if block.is_latest() || block.is_pending() {
            let parent_block = self.storage.replay_storage().latest_record();
            let parent_context = self
                .storage
                .replay_storage()
                .get_context(parent_block)
                .ok_or(RpcStorageError::BlockNotFound(block))?;
            return Ok(SimulationStartContext {
                block_context: self.build_pending_block_context(),
                parent_block_number: parent_block,
                parent_timestamp: parent_context.timestamp,
            });
        }

        let parent_block = self
            .storage
            .resolve_block_number(block)?
            .ok_or(RpcStorageError::BlockNotFound(block))?;
        let parent_context = self
            .storage
            .replay_storage()
            .get_context(parent_block)
            .ok_or(RpcStorageError::BlockNotFound(block))?;
        let parent = self
            .storage
            .get_block_by_id(block)?
            .ok_or(RpcStorageError::BlockNotFound(block))?;
        let block_context = next_block_context(parent_context, parent.hash());

        Ok(SimulationStartContext {
            block_context,
            parent_block_number: parent_block,
            parent_timestamp: parent_context.timestamp,
        })
    }

    fn create_simulation_txs<V: ViewState>(
        &self,
        calls: Vec<TransactionRequest>,
        block_context: BlockContext,
        mut state_view: V,
    ) -> Result<Vec<ZkTransaction>, EthCallError> {
        let default_gas_limit = simulation_default_gas_limit(
            &calls,
            block_context.gas_limit,
            self.config.eth_call_gas as u64,
        )?;
        let mut next_nonces = HashMap::new();

        calls
            .into_iter()
            .map(|mut request| {
                use alloy::network::TransactionBuilder;

                if request.gas.is_none() {
                    request.set_gas_limit(default_gas_limit);
                }

                let from = request.from.unwrap_or_default();
                let state_nonce = next_nonces.entry(from).or_insert_with(|| {
                    state_view
                        .get_account(from)
                        .as_ref()
                        .map(get_nonce)
                        .unwrap_or_default()
                });
                if let Some(nonce) = request.nonce {
                    if nonce >= *state_nonce {
                        *state_nonce =
                            nonce
                                .checked_add(1)
                                .ok_or(EthCallError::SimulateInvalidParams(
                                    "nonce has max value".to_string(),
                                ))?;
                    }
                } else {
                    let nonce = *state_nonce;
                    request.set_nonce(nonce);
                    *state_nonce =
                        nonce
                            .checked_add(1)
                            .ok_or(EthCallError::SimulateInvalidParams(
                                "nonce has max value".to_string(),
                            ))?;
                }

                self.create_tx_from_request(request, &block_context, false)
            })
            .collect()
    }
}

#[derive(Debug)]
struct SimulationStartContext {
    block_context: BlockContext,
    parent_block_number: u64,
    parent_timestamp: u64,
}

fn build_simulated_block_response(
    block_context: BlockContext,
    txs: Vec<ZkTransaction>,
    block_output: BlockOutput,
    return_full_transactions: bool,
) -> Result<(SimulatedBlock<ZkApiBlock>, OwnedOverrides), EthCallError> {
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

    Ok((
        SimulatedBlock { inner, calls },
        OwnedOverrides::new(
            storage_writes
                .into_iter()
                .map(|write| (write.key, write.value))
                .collect(),
            published_preimages.into_iter().collect(),
        ),
    ))
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

fn next_block_context(mut block_context: BlockContext, parent_hash: B256) -> BlockContext {
    block_context.block_number += 1;
    block_context.timestamp += 1;
    block_context.block_hashes.0.rotate_left(1);
    block_context.block_hashes.0[255] = U256::from_be_bytes(parent_hash.0);
    block_context
}

fn apply_simulate_block_overrides(
    block_context: &mut BlockContext,
    overrides: BlockOverrides,
    previous_block_number: u64,
    previous_timestamp: u64,
    max_block_gas_limit: u64,
) -> Result<(), EthCallError> {
    // Destructure to force a compile error when alloy adds new override fields, so we
    // explicitly decide whether to support or ignore each one.
    let BlockOverrides {
        number,
        time,
        gas_limit,
        coinbase,
        random,
        base_fee,
        blob_base_fee,
        block_hash,
        // ZKsync OS uses mix_hash for prevrandao and has no separate difficulty field; ignored.
        difficulty: _,
    } = overrides;

    if let Some(number) = number {
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
    if let Some(time) = time {
        if time <= previous_timestamp {
            return Err(EthCallError::SimulateBlockTimestampInvalid {
                got: time,
                parent: previous_timestamp,
            });
        }
        block_context.timestamp = time;
    }
    if let Some(gas_limit) = gas_limit {
        if max_block_gas_limit != 0 && gas_limit > max_block_gas_limit {
            return Err(EthCallError::SimulateBlockGasLimitExceeded);
        }
        block_context.gas_limit = gas_limit;
    }
    if let Some(coinbase) = coinbase {
        block_context.coinbase = coinbase;
    }
    if let Some(random) = random {
        block_context.mix_hash = U256::from_be_bytes(random.0);
    }
    if let Some(base_fee) = base_fee {
        block_context.eip1559_basefee = base_fee;
    }
    if let Some(blob_base_fee) = blob_base_fee {
        block_context.blob_fee = blob_base_fee;
    }
    if let Some(block_hash_overrides) = block_hash {
        let range_start = block_context.block_number.saturating_sub(256);
        for (block_number, block_hash) in
            block_hash_overrides.range(range_start..block_context.block_number)
        {
            let distance = block_context.block_number - block_number;
            let index = 256 - distance as usize;
            block_context.block_hashes.0[index] = U256::from_be_bytes(block_hash.0);
        }
    }

    Ok(())
}

fn simulation_default_gas_limit(
    calls: &[TransactionRequest],
    block_gas_limit: u64,
    per_call_gas_cap: u64,
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

    // Cap the per-call default at `per_call_gas_cap` to avoid handing a single call the entire
    // block's gas when the block limit is large and few calls specify gas explicitly.
    Ok(((block_gas_limit - total_specified_gas) / calls_without_gas).min(per_call_gas_cap))
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
                let (return_data, error) = match &output.execution_result {
                    ExecutionResult::Success(
                        ExecutionOutput::Call(return_bytes)
                        | ExecutionOutput::Create(return_bytes, _),
                    ) => (Bytes::from(return_bytes.clone()), None),
                    ExecutionResult::Revert(return_bytes) => {
                        let return_data = Bytes::from(return_bytes.clone());
                        (
                            return_data.clone(),
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
                    status: error.is_none(),
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
            Some(&self.tx_meta(block_hash, block_context)),
        )
    }

    fn api_logs(
        &self,
        block_hash: B256,
        block_context: BlockContext,
        receipt: &ZkReceiptEnvelope,
    ) -> Vec<alloy::rpc::types::Log> {
        let tx_hash = *self.tx.hash();
        let meta = self.tx_meta(block_hash, block_context);
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
            gas_used: self.gas_used(),
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
            0,
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
            0,
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
            0,
        )
        .unwrap();

        assert_eq!(context.block_number, 14);
        assert_eq!(context.block_hashes.0[252], U256::from(256));
        assert_eq!(context.block_hashes.0[253], U256::ZERO);
        assert_eq!(context.block_hashes.0[254], U256::ZERO);
        assert_eq!(context.block_hashes.0[255], U256::ZERO);
    }
}
