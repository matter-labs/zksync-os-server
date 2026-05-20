use crate::call_fees::{CallFees, CallFeesError};
use crate::config::RpcConfig;
use crate::js_tracer;
use crate::metrics::API_METRICS;
use crate::result::RevertError;
use crate::rpc_storage::{ReadRpcStorage, RpcStorageError};
use crate::sandbox::{call_trace_simulate, execute, execute_with};
use alloy::consensus::transaction::Recovered;
use alloy::consensus::{SignableTransaction, TxEip1559, TxEip2930, TxLegacy, TxType};
use alloy::eips::BlockId;
use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, B256, Bytes, Signature, TxKind, U256};
use alloy::rpc::types::state::StateOverride;
use alloy::rpc::types::trace::geth::{CallConfig, GethTrace};
use alloy::rpc::types::{BlockOverrides, TransactionRequest};
use derive_more::Deref;
use serde_json::Value as JsonValue;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::watch;
use zk_os_api::helpers::get_nonce;
use zksync_os_interface::types::{BlockHashes, ExecutionOutput};
use zksync_os_interface::{
    error::InvalidTransaction,
    types::{ExecutionResult, TxOutput},
};
use zksync_os_storage_api::{
    BlockContext, RepositoryError, StateError, ViewState, state_override_view::OverriddenStateView,
};
use zksync_os_tx_validators::policy_client::{AccessType, PolicyClient, PolicySession};
use zksync_os_types::ZksyncOsEncode;
use zksync_os_types::{
    L1_TX_MINIMAL_GAS_LIMIT, L1Envelope, L1PriorityTxType, L1Tx, L1TxType, L2Envelope,
    REQUIRED_L1_TO_L2_GAS_PER_PUBDATA_BYTE, SYSTEM_TX_TYPE_ID, UpgradeTxType, ZkEnvelope,
    ZkTransaction, ZkTxType,
};

#[derive(Clone, Debug)]
pub struct EthCallHandler<RpcStorage> {
    pub(crate) config: RpcConfig,
    pub(crate) storage: RpcStorage,
    pub(crate) chain_id: u64,
    /// Last block context constructed by sequencer but not necessarily executed yet.
    last_constructed_block_context: watch::Receiver<Option<BlockContext>>,
    /// Optional policy client. When set, `eth_call` and `eth_estimateGas`
    /// route their simulation through it (admit + judge fired by the
    /// bootloader inside `simulate_tx`). Block-build remains authoritative.
    policy_client: Option<PolicyClient>,
}

struct ExecutionEnv {
    block_context: BlockContext,
    transaction: ZkTransaction,
}

/// Builds new block context for theoretical pending block using current system state.
pub(crate) fn build_pending_block_context(
    storage: &impl ReadRpcStorage,
    chain_id: u64,
) -> BlockContext {
    let latest_block_number = storage.replay_storage().latest_record();
    let latest_block = storage
        .replay_storage()
        .get_replay_record(latest_block_number)
        .expect("latest block record must exist");
    let latest_block_context = latest_block.block_context;

    // Shift block hashes one to the left and append latest block's hash
    let mut block_hashes = latest_block_context.block_hashes.0;
    block_hashes.rotate_left(1);
    block_hashes[255] = U256::from_be_bytes(latest_block.block_output_hash.0);

    // Use current timestamp for pending block
    let millis_since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("incorrect system time")
        .as_millis();
    let timestamp = (millis_since_epoch / 1000) as u64;

    BlockContext {
        chain_id,
        block_number: latest_block_number + 1,
        block_hashes: BlockHashes(block_hashes),
        timestamp,
        // Presume all other fields are the same as latest block, subject to change in the future
        eip1559_basefee: latest_block_context.eip1559_basefee,
        pubdata_price: latest_block_context.pubdata_price,
        native_price: latest_block_context.native_price,
        coinbase: latest_block_context.coinbase,
        gas_limit: latest_block_context.gas_limit,
        pubdata_limit: latest_block_context.pubdata_limit,
        mix_hash: latest_block_context.mix_hash,
        execution_version: latest_block_context.execution_version,
        blob_fee: latest_block_context.blob_fee,
    }
}

impl<RpcStorage: ReadRpcStorage> EthCallHandler<RpcStorage> {
    pub fn new(
        config: RpcConfig,
        storage: RpcStorage,
        chain_id: u64,
        last_constructed_block_context: watch::Receiver<Option<BlockContext>>,
        policy_client: Option<PolicyClient>,
    ) -> Self {
        Self {
            config,
            storage,
            chain_id,
            last_constructed_block_context,
            policy_client,
        }
    }

    pub(crate) fn create_tx_from_request(
        &self,
        request: TransactionRequest,
        block_context: &BlockContext,
        for_estimate_gas: bool,
    ) -> Result<ZkTransaction, EthCallError> {
        let tx_type = request.minimal_tx_type();

        let TransactionRequest {
            from,
            to,
            gas_price,
            max_fee_per_gas,
            max_priority_fee_per_gas,
            gas,
            value,
            input,
            nonce,
            access_list,
            chain_id,
            // todo(EIP-4844)
            blob_versioned_hashes: _,
            max_fee_per_blob_gas: _,
            sidecar: _,
            // todo(EIP-7702)
            authorization_list: _,
            // EIP-2718 transaction type - ignored
            transaction_type: _,
        } = request;

        let gas_limit = gas.unwrap_or(self.config.eth_call_gas as u64);
        let nonce = if let Some(nonce) = nonce {
            nonce
        } else {
            self.storage
                .state_at_block_number_or_latest(block_context.block_number)?
                .get_account(from.unwrap_or_default())
                .as_ref()
                .map(get_nonce)
                .unwrap_or_default()
        };

        let CallFees {
            max_priority_fee_per_gas,
            gas_price,
        } = CallFees::ensure_fees(
            gas_price,
            max_fee_per_gas,
            max_priority_fee_per_gas,
            block_context.eip1559_basefee.saturating_to(),
            for_estimate_gas,
        )?;
        let chain_id = chain_id.unwrap_or(self.chain_id);
        let from = from.unwrap_or_default();
        let to = to.unwrap_or(TxKind::Create);
        let value = value.unwrap_or_default();
        let input = input.into_input().unwrap_or_default();

        // Mock signature as this is a simulated transaction
        let signature = Signature::new(Default::default(), Default::default(), false);

        match request.transaction_type {
            Some(L1PriorityTxType::TX_TYPE) => {
                let inner = L1Tx {
                    hash: B256::ZERO,
                    initiator: from,
                    to: to.into_to().unwrap_or_default(),
                    gas_limit: request.gas.unwrap_or(self.config.eth_call_gas as u64),
                    gas_per_pubdata_byte_limit: REQUIRED_L1_TO_L2_GAS_PER_PUBDATA_BYTE,
                    max_fee_per_gas: gas_price,
                    max_priority_fee_per_gas: max_priority_fee_per_gas.unwrap_or_default(),
                    nonce,
                    value,
                    to_mint: value + U256::from(gas_price) * U256::from(gas_limit),
                    refund_recipient: Address::default(),
                    input,
                    factory_deps: vec![],
                    marker: std::marker::PhantomData::<L1PriorityTxType>,
                };
                return Ok(L1Envelope { inner }.into());
            }
            Some(UpgradeTxType::TX_TYPE) => {
                return Err(EthCallError::UpgradeTxNotEstimatable);
            }
            Some(SYSTEM_TX_TYPE_ID) => {
                return Err(EthCallError::SystemTxNotEstimatable);
            }
            _ => {}
        }

        // Build each transaction type manually to enforce proper handling of all involved fields.
        // Arguably this is too verbose, but this way we can clearly see which fields are expected to
        // be present in all supported transaction types.
        let tx = match tx_type {
            TxType::Legacy => L2Envelope::from(
                TxLegacy {
                    chain_id: Some(chain_id),
                    nonce,
                    gas_price,
                    gas_limit,
                    to,
                    value,
                    input,
                }
                .into_signed(signature),
            ),
            TxType::Eip2930 => L2Envelope::from(
                TxEip2930 {
                    chain_id,
                    nonce,
                    gas_price,
                    gas_limit,
                    to,
                    value,
                    input,
                    access_list: access_list.unwrap_or_default(),
                }
                .into_signed(signature),
            ),
            TxType::Eip1559 => L2Envelope::from(
                TxEip1559 {
                    chain_id,
                    nonce,
                    max_priority_fee_per_gas: max_priority_fee_per_gas
                        .ok_or(EthCallError::MissingPriorityFee)?,
                    max_fee_per_gas: gas_price,
                    gas_limit,
                    to,
                    value,
                    input,
                    access_list: access_list.unwrap_or_default(),
                }
                .into_signed(signature),
            ),
            TxType::Eip4844 => {
                return Err(EthCallError::Eip4844NotSupported);
            }
            TxType::Eip7702 => {
                return Err(EthCallError::Eip7702NotSupported);
            }
        };
        Ok(Recovered::new_unchecked(tx, from).into())
    }

    fn resolve_block_context(
        &self,
        block_id: Option<BlockId>,
    ) -> Result<BlockContext, EthCallError> {
        let block_id = block_id.unwrap_or_default();
        if block_id.is_pending() {
            let latest_block_number = self.storage.replay_storage().latest_record();
            // Check if last constructed block context has been fully processed yet
            if let Some(pending_block_context) = *self.last_constructed_block_context.borrow()
                && pending_block_context.block_number > latest_block_number
            {
                // If it hasn't, it's the pending block we are looking for
                Ok(pending_block_context)
            } else {
                // If it has, we build new block context using current system state
                Ok(build_pending_block_context(&self.storage, self.chain_id))
            }
        } else {
            let Some(block_number) = self.storage.resolve_block_number(block_id)? else {
                return Err(RpcStorageError::BlockNotFound(block_id).into());
            };
            self.storage
                .replay_storage()
                .get_context(block_number)
                .ok_or(RpcStorageError::BlockNotFound(block_id).into())
        }
    }

    fn prepare_execution_env(
        &self,
        request: TransactionRequest,
        block: Option<BlockId>,
        block_overrides: Option<Box<BlockOverrides>>,
    ) -> Result<ExecutionEnv, EthCallError> {
        if block_overrides.is_some() {
            return Err(EthCallError::BlockOverridesNotSupported);
        }

        let block_context = self.resolve_block_context(block)?;
        let transaction = self.create_tx_from_request(request, &block_context, false)?;

        Ok(ExecutionEnv {
            transaction,
            block_context,
        })
    }

    pub fn call_impl(
        &self,
        request: TransactionRequest,
        block: Option<BlockId>,
        state_overrides: Option<StateOverride>,
        block_overrides: Option<Box<BlockOverrides>>,
    ) -> Result<Bytes, EthCallError> {
        let mut execution_env = self.prepare_execution_env(request, block, block_overrides)?;
        execution_env.block_context.eip1559_basefee = U256::from(0);

        let storage_view = self
            .storage
            .state_at_block_number_or_latest(execution_env.block_context.block_number)?;
        let state_view = OverriddenStateView::with_state_overrides(
            storage_view,
            state_overrides.unwrap_or_default(),
        );

        let tx_type = execution_env.transaction.tx_type();
        // New session per call so concurrent simulations don't share captured
        // frames. Read intent because `eth_call` is read-only.
        let mut policy_session = self
            .policy_client
            .as_ref()
            .filter(|_| tx_type_runs_policy(tx_type))
            .map(|client| client.session(AccessType::Read));
        let res = simulate_with_optional_policy(
            execution_env.transaction,
            execution_env.block_context,
            state_view,
            policy_session.as_mut(),
        )
        .map_err(EthCallError::ForwardSubsystemError)?
        .map_err(map_simulate_invalid_to_call_error)?;

        API_METRICS.call_gas_used[&"eth_call".to_string()].observe(res.gas_used);

        match res.execution_result {
            ExecutionResult::Success(
                ExecutionOutput::Call(return_bytes) | ExecutionOutput::Create(return_bytes, _),
            ) => Ok(Bytes::from(return_bytes)),
            ExecutionResult::Revert(return_bytes) => {
                let error = RevertError::new(Bytes::from(return_bytes));
                Err(EthCallError::Revert(error))?
            }
        }
    }

    pub fn call_trace_impl(
        &self,
        request: TransactionRequest,
        block: Option<BlockId>,
        call_config: CallConfig,
        state_overrides: Option<StateOverride>,
        block_overrides: Option<Box<BlockOverrides>>,
    ) -> Result<GethTrace, EthCallError> {
        let execution_env = self.prepare_execution_env(request, block, block_overrides)?;
        let storage_view = self
            .storage
            .state_at_block_number_or_latest(execution_env.block_context.block_number)?;

        match state_overrides {
            Some(overrides) => call_trace_simulate(
                execution_env.transaction,
                execution_env.block_context,
                OverriddenStateView::with_state_overrides(storage_view, overrides),
                call_config,
            ),
            None => call_trace_simulate(
                execution_env.transaction,
                execution_env.block_context,
                storage_view,
                call_config,
            ),
        }
        .map(GethTrace::CallTracer)
        .map_err(|err| EthCallError::ForwardSubsystemError(anyhow::anyhow!(err)))
    }

    pub fn call_js_tracer_impl(
        &self,
        request: TransactionRequest,
        block: Option<BlockId>,
        js_cfg: String,
        state_overrides: Option<StateOverride>,
        block_overrides: Option<Box<BlockOverrides>>,
    ) -> Result<JsonValue, EthCallError> {
        let execution_env = self.prepare_execution_env(request, block, block_overrides)?;
        let storage_view = self
            .storage
            .state_at_block_number_or_latest(execution_env.block_context.block_number)?;

        let mut tracer_output = match state_overrides {
            Some(overrides) => {
                let view = OverriddenStateView::with_state_overrides(storage_view, overrides);
                let mut tracer = js_tracer::tracer::JsTracer::new(view.clone(), js_cfg)
                    .map_err(|e| EthCallError::ForwardSubsystemError(anyhow::anyhow!(e)))?;

                zksync_os_multivm::simulate_tx(
                    execution_env.transaction.encode(),
                    execution_env.block_context,
                    view.clone(),
                    view,
                    &mut tracer,
                    &mut zksync_os_interface::tracing::NopValidator,
                )
                .map_err(|e| EthCallError::ForwardSubsystemError(anyhow::anyhow!(e)))
                .and_then(|inner| inner.map_err(EthCallError::InvalidTransaction))?;

                tracer
            }
            None => {
                let mut tracer = js_tracer::tracer::JsTracer::new(storage_view.clone(), js_cfg)
                    .map_err(|e| EthCallError::ForwardSubsystemError(anyhow::anyhow!(e)))?;

                zksync_os_multivm::simulate_tx(
                    execution_env.transaction.encode(),
                    execution_env.block_context,
                    storage_view.clone(),
                    storage_view,
                    &mut tracer,
                    &mut zksync_os_interface::tracing::NopValidator,
                )
                .map_err(|e| EthCallError::ForwardSubsystemError(anyhow::anyhow!(e)))
                .and_then(|inner| inner.map_err(EthCallError::InvalidTransaction))?;

                tracer
            }
        };

        if let Some(err) = tracer_output.take_error() {
            return Err(EthCallError::CallTracerError(err));
        }

        Ok(tracer_output.results.pop().unwrap_or(JsonValue::Null))
    }

    pub fn estimate_gas_impl(
        &self,
        request: TransactionRequest,
        block: Option<BlockId>,
        state_override: Option<StateOverride>,
    ) -> Result<U256, EthCallError> {
        let mut block_context = self.resolve_block_context(block)?;

        // Overestimate pubdata price to leave some space for fluctuations. Usual Ethereum tooling
        // assumes that gas limit stays constant in most scenarios, which is not the case in our system.
        block_context.pubdata_price = U256::from(
            f64::from(block_context.pubdata_price) * self.config.estimate_gas_pubdata_price_factor,
        );

        // Choose storage view (with optional overrides) once and reuse it throughout.
        let storage_view = self
            .storage
            .state_at_block_number_or_latest(block_context.block_number)?;
        match state_override {
            Some(overrides) => self.estimate_gas_with_view(
                request,
                block_context,
                OverriddenStateView::with_state_overrides(storage_view, overrides),
            ),
            None => self.estimate_gas_with_view(request, block_context, storage_view),
        }
    }
}

impl<RpcStorage: ReadRpcStorage> EthCallHandler<RpcStorage> {
    fn build_estimate_tx<V: ViewState>(
        &self,
        mut request: TransactionRequest,
        block_context: &BlockContext,
        storage_view: &mut V,
    ) -> Result<ZkTransaction, EthCallError> {
        let block_gas_limit = block_context.gas_limit;
        let mut highest_gas_limit = request.gas.unwrap_or(block_gas_limit).min(block_gas_limit);

        let effective_gas_price = request
            .gas_price
            .or(request.max_fee_per_gas)
            .unwrap_or_default();
        if effective_gas_price > 0 {
            let gas_limit_from_balance =
                max_gas_from_balance(&request, block_context.eip1559_basefee, storage_view)?;
            highest_gas_limit = highest_gas_limit.min(gas_limit_from_balance);
        }
        request.set_gas_limit(highest_gas_limit);
        self.create_tx_from_request(request, block_context, true)
    }

    // The flow was heavily borrowed from reth, which in turn closely follows the original geth logic. Source:
    // https://github.com/paradigmxyz/reth/blob/5bc8589162b6e23b07919d82a57eee14353f2862/crates/rpc/rpc-eth-api/src/helpers/estimate.rs
    fn estimate_gas_with_view<V: ViewState + Clone>(
        &self,
        request: TransactionRequest,
        block_context: BlockContext,
        mut storage_view: V,
    ) -> Result<U256, EthCallError> {
        tracing::trace!("Estimating gas with block context {block_context:?}");

        let tx = self.build_estimate_tx(request, &block_context, &mut storage_view)?;

        let run_at = |gas_limit: u64| {
            let mut attempt = tx.clone();
            set_gas_limit(&mut attempt, gas_limit);
            execute(attempt, block_context, storage_view.clone())
                .map_err(EthCallError::ForwardSubsystemError)
        };

        // Execute the transaction with the highest possible gas limit.
        let res = run_at(tx.gas_limit())?.map_err(EthCallError::InvalidTransaction)?;
        tracing::trace!(
            "Executed tx in eth_estimateGas with gas limit: {:?}, result {res:?}",
            Probe::Highest(tx.gas_limit())
        );
        if let ExecutionResult::Revert(output) = res.execution_result {
            return Err(EthCallError::Revert(RevertError::new(Bytes::from(output))));
        }

        // NOTE: this is the gas the transaction used, which is less than the transaction requires to succeed.
        let gas_used = res.gas_used;
        let mut range = GasRange::new(gas_used.saturating_sub(1), tx.gas_limit());

        if tx.tx_type() == ZkTxType::L1 {
            range.apply_floor(L1_TX_MINIMAL_GAS_LIMIT);
        }

        // Optimistic check: tx likely passes at gas_used + refund + stipend, scaled by 64/63 (EIP-150).
        // <https://github.com/ethereum/go-ethereum/blob/a5a4fa7032bb248f5a7c40f4e8df2b131c4186a4/eth/gasestimator/gasestimator.go#L135>
        const GAS_STIPEND: u64 = 2_300;
        let optimistic_gas_limit = (gas_used + res.gas_refunded + GAS_STIPEND) * 64 / 63;
        if optimistic_gas_limit > range.lowest && optimistic_gas_limit < range.highest {
            range.apply_probe(
                run_at(optimistic_gas_limit)?,
                Probe::Optimistic(optimistic_gas_limit),
            )?;
        }

        // Binary search narrows the range to find the minimum gas limit needed for the transaction
        // to succeed.
        // <https://github.com/ethereum/go-ethereum/blob/a5a4fa7032bb248f5a7c40f4e8df2b131c4186a4/eth/gasestimator/gasestimator.go#L152>
        let mut mid = range.biased_midpoint();
        while !range.is_narrow_enough() {
            tracing::trace!("Trying to simulate transaction with gas_limit {mid}");
            range.apply_probe(run_at(mid)?, Probe::Midpoint(mid))?;
            mid = range.midpoint();
        }
        tracing::trace!("Estimated gas limit: {}", range.highest);

        // Re-execute the resolved gas limit once with the validator wired in.
        // The binary search runs without the validator (one round-trip per
        // iteration would be 30+ calls). `Write` intent: gas is
        // state-dependent, so a read-only caller estimating gas would
        // sidechannel state.
        if let Some(policy_client) = &self.policy_client
            && tx_type_runs_policy(tx.tx_type())
        {
            let mut judged_tx = tx.clone();
            set_gas_limit(&mut judged_tx, range.highest);
            let mut policy_session = policy_client.session(AccessType::Write);
            simulate_with_optional_policy(
                judged_tx,
                block_context,
                storage_view,
                Some(&mut policy_session),
            )
            .map_err(EthCallError::ForwardSubsystemError)?
            .map_err(map_simulate_invalid_to_call_error)?;
        }

        Ok(U256::from(range.highest))
    }

    pub fn last_constructed_block_context(&self) -> Option<BlockContext> {
        *self.last_constructed_block_context.borrow()
    }
}

/// Simulate `tx`, optionally wiring `policy` as the validator. With a
/// validator, the bootloader fires admit + judge inline; without, the
/// simulation runs with `NopValidator` + `NopTracer`.
fn simulate_with_optional_policy<V: ViewState>(
    tx: ZkTransaction,
    block_context: BlockContext,
    view: V,
    policy: Option<&mut PolicySession>,
) -> anyhow::Result<Result<TxOutput, InvalidTransaction>> {
    if let Some(policy) = policy {
        let mut tracer = policy.paired_tracer();
        execute_with(tx, block_context, view, &mut tracer, policy)
    } else {
        execute(tx, block_context, view)
    }
}

/// Surface validator denials as `PolicyDenied` so the rpc layer maps them
/// to `TransactionRejected` rather than a generic invalid-transaction error.
fn map_simulate_invalid_to_call_error(err: InvalidTransaction) -> EthCallError {
    match err {
        InvalidTransaction::FilteredByValidator => EthCallError::PolicyDenied,
        _ => EthCallError::InvalidTransaction(err),
    }
}

/// L1 priority and upgrade txs bypass the validator end-to-end (block-build
/// doesn't fire it on them either). Exhaustive match (no `_` arm) so a
/// future `ZkTxType` variant can't silently bypass the policy.
fn tx_type_runs_policy(tx_type: ZkTxType) -> bool {
    match tx_type {
        ZkTxType::L2(_) => true,
        ZkTxType::L1 | ZkTxType::Upgrade | ZkTxType::System => false,
    }
}

#[derive(Debug, Deref)]
enum Probe {
    Midpoint(u64),
    Highest(u64),
    Optimistic(u64),
}

const ESTIMATE_GAS_ERROR_RATIO: f64 = 0.015;

fn is_out_of_gas(err: &InvalidTransaction) -> bool {
    matches!(
        err,
        InvalidTransaction::CallGasCostMoreThanGasLimit
            | InvalidTransaction::OutOfGasDuringValidation
            | InvalidTransaction::OutOfNativeResourcesDuringValidation
    )
}

// Invariant: tx fails at `lowest` (false), tx succeeds at `highest` (true).
struct GasRange {
    lowest: u64,
    highest: u64,
}

impl GasRange {
    fn new(lowest: u64, highest: u64) -> Self {
        Self { lowest, highest }
    }

    fn is_narrow_enough(&self) -> bool {
        (self.highest - self.lowest) as f64 / (self.highest as f64) < ESTIMATE_GAS_ERROR_RATIO
    }

    fn midpoint(&self) -> u64 {
        u64::midpoint(self.lowest, self.highest)
    }

    fn biased_midpoint(&self) -> u64 {
        (self.lowest * 3).min(self.midpoint())
    }

    fn apply_floor(&mut self, floor: u64) {
        self.lowest = self.lowest.max(floor);
        self.highest = self.highest.max(floor);
    }

    fn apply_probe(
        &mut self,
        result: Result<TxOutput, InvalidTransaction>,
        probe: Probe,
    ) -> Result<(), EthCallError> {
        if result.as_ref().is_err_and(is_out_of_gas) {
            self.lowest = *probe;
            return Ok(());
        }
        let res = result.map_err(EthCallError::InvalidTransaction)?;
        tracing::trace!("Executed tx in eth_estimateGas with gas limit: {probe:?}, result {res:?}");
        match res.execution_result {
            ExecutionResult::Success(_) => self.highest = *probe,
            ExecutionResult::Revert(_) => self.lowest = *probe,
        }
        Ok(())
    }
}

fn set_gas_limit(tx: &mut ZkTransaction, gas_limit: u64) {
    match tx.inner.inner_mut() {
        ZkEnvelope::System(_) => {
            unreachable!("system transactions don't have explicit gas limit");
        }
        ZkEnvelope::L2(L2Envelope::Legacy(inner)) => inner.tx_mut().gas_limit = gas_limit,
        ZkEnvelope::L2(L2Envelope::Eip2930(inner)) => inner.tx_mut().gas_limit = gas_limit,
        ZkEnvelope::L2(L2Envelope::Eip1559(inner)) => inner.tx_mut().gas_limit = gas_limit,
        ZkEnvelope::L2(L2Envelope::Eip4844(inner)) => inner.tx_mut().as_mut().gas_limit = gas_limit,
        ZkEnvelope::L2(L2Envelope::Eip7702(inner)) => inner.tx_mut().gas_limit = gas_limit,
        ZkEnvelope::L1(envelope) => {
            let tx = &mut envelope.inner;
            tx.gas_limit = gas_limit;
            tx.to_mint = tx.value + U256::from(tx.max_fee_per_gas) * U256::from(gas_limit);
        }
        ZkEnvelope::Upgrade(envelope) => envelope.inner.gas_limit = gas_limit,
    }
}

/// Returns how much gas the sender can afford: `(balance - value) / gas_price`.
fn max_gas_from_balance<V: ViewState>(
    request: &TransactionRequest,
    gas_price: U256,
    storage_view: &mut V,
) -> Result<u64, EthCallError> {
    let balance = storage_view.balance(request.from.unwrap_or_default());

    let value = request.value.unwrap_or_default();
    let balance = balance
        .checked_sub(value)
        .ok_or(EthCallError::InvalidTransaction(
            InvalidTransaction::LackOfFundForMaxFee {
                fee: value,
                balance,
            },
        ))?;

    Ok(balance
        .checked_div(gas_price)
        .unwrap_or_default()
        .saturating_to())
}

/// Error types returned by `eth_call` implementation
#[derive(Debug, thiserror::Error)]
pub enum EthCallError {
    /// Policy service rejected the simulation request at the RPC admit
    /// boundary.
    #[error("simulation denied by policy service")]
    PolicyDenied,
    // todo: temporary, needs to be supported eventually
    #[error("block overrides are not supported in `eth_call`")]
    BlockOverridesNotSupported,
    #[error("invalid `eth_simulateV1` params: {0}")]
    SimulateInvalidParams(String),
    #[error("invalid block override in `eth_simulateV1`: {0}")]
    SimulateInvalidBlockOverride(&'static str),
    #[error("block numbers must be in order: {got} <= {parent}")]
    SimulateBlockNumberInvalid { got: u64, parent: u64 },
    #[error("block timestamps must be in order: {got} <= {parent}")]
    SimulateBlockTimestampInvalid { got: u64, parent: u64 },
    #[error("block gas limit exceeded by the block's transactions")]
    SimulateBlockGasLimitExceeded,
    #[error("movePrecompileToAddress is not supported by this execution backend")]
    SimulateMovePrecompileNotSupported,
    // todo(EIP-4844)
    #[error("EIP-4844 transactions are not supported")]
    Eip4844NotSupported,
    // todo(EIP-7702)
    #[error("EIP-7702 transactions are not supported")]
    Eip7702NotSupported,
    #[error("upgrade transactions cannot be estimated")]
    UpgradeTxNotEstimatable,
    #[error("system transactions cannot be estimated")]
    SystemTxNotEstimatable,

    /// Error while decoding or validating transaction request fees.
    #[error(transparent)]
    CallFees(#[from] CallFeesError),
    /// Missing a mandatary field `maxPriorityFeePerGas`. Only returned if transaction's minimal
    /// buildable type enforces this field to be present (i.e., not legacy or EIP-2930).
    #[error("missing `maxPriorityFeePerGas` field for EIP-1559 transaction")]
    MissingPriorityFee,

    /// Thrown if executing a transaction failed during estimate/call
    #[error("execution reverted: {0}")]
    Revert(RevertError),

    // Below is more or less temporary as the error hierarchy in ZKsync OS is going through a major
    // refactoring.
    /// Internal error propagated by ZKsync OS. Boxed due to its large size.
    #[error("ZKsync OS error: {0:?}")]
    ForwardSubsystemError(anyhow::Error),
    /// Transaction is invalid according to ZKsync OS.
    #[error("invalid transaction: {0:?}")]
    InvalidTransaction(InvalidTransaction),

    #[error(transparent)]
    Storage(#[from] RpcStorageError),
    #[error(transparent)]
    Repository(#[from] RepositoryError),
    #[error(transparent)]
    State(#[from] StateError),

    /// Error occurred during debug tracing
    #[error("Tracer error: {0:?}")]
    CallTracerError(anyhow::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tx_type_runs_policy_only_for_l2_variants() {
        use alloy::consensus::TxType;
        assert!(tx_type_runs_policy(ZkTxType::L2(TxType::Legacy.into())));
        assert!(tx_type_runs_policy(ZkTxType::L2(TxType::Eip1559.into())));
        assert!(!tx_type_runs_policy(ZkTxType::L1));
        assert!(!tx_type_runs_policy(ZkTxType::Upgrade));
        assert!(!tx_type_runs_policy(ZkTxType::System));
    }
}
