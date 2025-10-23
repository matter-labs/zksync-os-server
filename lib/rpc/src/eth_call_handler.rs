use crate::call_fees::{CallFees, CallFeesError};
use crate::config::RpcConfig;
use crate::result::RevertError;
use crate::rpc_storage::ReadRpcStorage;
use crate::sandbox::{call_trace_simulate, execute};
use alloy::consensus::transaction::Recovered;
use alloy::consensus::{SignableTransaction, TxEip1559, TxEip2930, TxLegacy, TxType};
use alloy::eips::BlockId;
use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, B256, Bytes, Signature, TxKind, U256};
use alloy::rpc::types::state::StateOverride;
use alloy::rpc::types::trace::geth::{CallConfig, GethTrace};
use alloy::rpc::types::{BlockOverrides, TransactionRequest};
use zk_os_api::helpers::{get_balance, get_nonce};
use zksync_os_interface::types::ExecutionOutput;
use zksync_os_interface::{
    error::InvalidTransaction,
    types::{BlockContext, ExecutionResult},
};
use zksync_os_storage_api::ViewState;
use zksync_os_storage_api::{
    RepositoryError, StateError, state_override_view::OverriddenStateView,
};
use zksync_os_types::{
    L1_TX_MINIMAL_GAS_LIMIT, L1Envelope, L1PriorityTxType, L1Tx, L1TxType, L2Envelope,
    REQUIRED_L1_TO_L2_GAS_PER_PUBDATA_BYTE, UpgradeTxType, ZkEnvelope, ZkTransaction, ZkTxType,
};

const ESTIMATE_GAS_ERROR_RATIO: f64 = 0.015;

#[derive(Clone, Debug)]
pub struct EthCallHandler<RpcStorage> {
    config: RpcConfig,
    storage: RpcStorage,
    chain_id: u64,
}

struct ExecutionEnv {
    block_context: BlockContext,
    transaction: ZkTransaction,
}

impl<RpcStorage: ReadRpcStorage> EthCallHandler<RpcStorage> {
    pub fn new(config: RpcConfig, storage: RpcStorage, chain_id: u64) -> Self {
        Self {
            config,
            storage,
            chain_id,
        }
    }

    fn create_tx_from_request(
        &self,
        request: TransactionRequest,
        block_context: &BlockContext,
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
                .state_view_at(block_context.block_number)?
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
        )?;
        let chain_id = chain_id.unwrap_or(self.chain_id);
        let from = from.unwrap_or_default();
        let to = to.unwrap_or(TxKind::Create);
        let value = value.unwrap_or_default();
        let input = input.into_input().unwrap_or_default();

        // Mock signature as this is a simulated transaction
        let signature = Signature::new(Default::default(), Default::default(), false);

        if request.transaction_type == Some(L1PriorityTxType::TX_TYPE) {
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
        } else if request.transaction_type == Some(UpgradeTxType::TX_TYPE) {
            return Err(EthCallError::UpgradeTxNotEstimatable);
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

    fn prepare_execution_env(
        &self,
        request: TransactionRequest,
        block: Option<BlockId>,
        block_overrides: Option<Box<BlockOverrides>>,
    ) -> Result<ExecutionEnv, EthCallError> {
        if block_overrides.is_some() {
            return Err(EthCallError::BlockOverridesNotSupported);
        }

        let block_id = block.unwrap_or_default();
        let Some(block_number) = self.storage.resolve_block_number(block_id)? else {
            return Err(EthCallError::BlockNotFound(block_id));
        };
        let block_context = self
            .storage
            .replay_storage()
            .get_context(block_number)
            .ok_or(EthCallError::BlockNotFound(block_id))?;

        let transaction = self.create_tx_from_request(request, &block_context)?;

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
        let execution_env = self.prepare_execution_env(request, block, block_overrides)?;
        let storage_view = self
            .storage
            .state_view_at(execution_env.block_context.block_number)?;

        let res = match state_overrides {
            Some(overrides) => execute(
                execution_env.transaction,
                execution_env.block_context,
                OverriddenStateView::new(storage_view, overrides),
            ),
            None => execute(
                execution_env.transaction,
                execution_env.block_context,
                storage_view,
            ),
        }
        .map_err(EthCallError::ForwardSubsystemError)?
        .map_err(EthCallError::InvalidTransaction)?;

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
            .state_view_at(execution_env.block_context.block_number)?;

        match state_overrides {
            Some(overrides) => call_trace_simulate(
                execution_env.transaction,
                execution_env.block_context,
                OverriddenStateView::new(storage_view, overrides),
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

    pub fn estimate_gas_impl(
        &self,
        request: TransactionRequest,
        block_number: Option<BlockId>,
        state_override: Option<StateOverride>,
    ) -> Result<U256, EthCallError> {
        let block_id = block_number.unwrap_or_default();
        let Some(block_number) = self.storage.resolve_block_number(block_id)? else {
            return Err(EthCallError::BlockNotFound(block_id));
        };
        let block_context = self
            .storage
            .replay_storage()
            .get_context(block_number)
            .ok_or(EthCallError::BlockNotFound(block_id))?;

        // Choose storage view (with optional overrides) once and reuse it throughout.
        let base_view = self.storage.state_view_at(block_number)?;
        match state_override {
            Some(overrides) => self.estimate_gas_with_view(
                request,
                block_context,
                OverriddenStateView::new(base_view, overrides),
            ),
            None => self.estimate_gas_with_view(request, block_context, base_view),
        }
    }
}

impl<RpcStorage: ReadRpcStorage> EthCallHandler<RpcStorage> {
    fn estimate_gas_with_view<V: ViewState>(
        &self,
        mut request: TransactionRequest,
        block_context: BlockContext,
        mut storage_view: V,
    ) -> Result<U256, EthCallError> {
        // Rest of the flow was heavily borrowed from reth, which in turn closely follows the
        // original geth logic. Source:
        // https://github.com/paradigmxyz/reth/blob/5bc8589162b6e23b07919d82a57eee14353f2862/crates/rpc/rpc-eth-api/src/helpers/estimate.rs

        // the gas limit of the corresponding block
        let block_gas_limit = block_context.gas_limit;

        // Determine the highest possible gas limit, considering both the request's specified limit
        // and the block's limit.
        let mut highest_gas_limit = request
            .gas
            .map(|mut tx_gas_limit| {
                if block_gas_limit < tx_gas_limit {
                    // requested gas limit is higher than the allowed gas limit, capping
                    tx_gas_limit = block_gas_limit;
                }
                tx_gas_limit
            })
            .unwrap_or(block_gas_limit);

        // Check funds of the sender (only useful to check if transaction gas price is more than 0).
        //
        // The caller allowance is check by doing `(account.balance - tx.value) / tx.gas_price`
        if request
            .gas_price
            .or(request.max_fee_per_gas)
            .unwrap_or_default()
            > 0
        {
            let balance = storage_view
                .get_account(request.from.unwrap_or_default())
                .as_ref()
                .map(get_balance)
                .unwrap_or_default();

            let value = request.value.unwrap_or_default();
            // Subtract transferred value from the caller balance. Return error if the caller has
            // insufficient funds.
            let balance = balance
                .checked_sub(value)
                .ok_or(EthCallError::InvalidTransaction(
                    InvalidTransaction::LackOfFundForMaxFee {
                        fee: value,
                        balance,
                    },
                ))?;
            // Cap the highest gas limit by max gas caller can afford with given gas price
            highest_gas_limit = highest_gas_limit.min(
                // Calculate the amount of gas the caller can afford with the specified gas price.
                balance
                    .checked_div(block_context.eip1559_basefee)
                    // This will be 0 if gas price is 0. It is fine, because we check it before.
                    .unwrap_or_default()
                    .saturating_to(),
            );
        }
        request.set_gas_limit(
            request
                .gas
                .unwrap_or(highest_gas_limit)
                .min(highest_gas_limit),
        );
        let tx = self.create_tx_from_request(request, &block_context)?;

        // Execute the transaction with the highest possible gas limit.
        let mut res = execute(tx.clone(), block_context, storage_view.clone())
            .map_err(EthCallError::ForwardSubsystemError)?
            .map_err(EthCallError::InvalidTransaction)?;
        match res.execution_result {
            ExecutionResult::Success(_) => {
                // Transaction succeeded with the highest possible gas limit, we can proceed with
                // binary search
            }
            ExecutionResult::Revert(output) => {
                let error = RevertError::new(Bytes::from(output));
                return Err(EthCallError::Revert(error));
            }
        }

        // we know the tx succeeded with the configured gas limit, so we can use that as the
        // highest, in case we applied a gas cap due to caller allowance above
        highest_gas_limit = tx.gas_limit();

        // NOTE: this is the gas the transaction used, which is less than the
        // transaction requires to succeed.
        let mut gas_used = res.gas_used;
        // the lowest value is capped by the gas used by the unconstrained transaction
        let mut lowest_gas_limit = gas_used.saturating_sub(1);

        // As stated in Geth, there is a good chance that the transaction will pass if we set the
        // gas limit to the execution gas used plus the gas refund, so we check this first
        // <https://github.com/ethereum/go-ethereum/blob/a5a4fa7032bb248f5a7c40f4e8df2b131c4186a4/eth/gasestimator/gasestimator.go#L135
        //
        // Calculate the optimistic gas limit by adding gas used and gas refund,
        // then applying a 64/63 multiplier to account for gas forwarding rules.
        let optimistic_gas_limit = (gas_used + res.gas_refunded + 2_300) * 64 / 63;
        if optimistic_gas_limit < highest_gas_limit {
            // Set the transaction's gas limit to the calculated optimistic gas limit.
            let mut optimistic_tx = tx.clone();
            set_gas_limit(&mut optimistic_tx, optimistic_gas_limit);

            // Re-execute the transaction with the new gas limit and update the result and
            // environment.
            res = execute(optimistic_tx, block_context, storage_view.clone())
                .map_err(EthCallError::ForwardSubsystemError)?
                .map_err(EthCallError::InvalidTransaction)?;

            // Update the gas used based on the new result.
            gas_used = res.gas_used;
            // Update the gas limit estimates (highest and lowest) based on the execution result.
            update_estimated_gas_range(
                res.execution_result,
                optimistic_gas_limit,
                &mut highest_gas_limit,
                &mut lowest_gas_limit,
            )?;
        };

        if tx.tx_type() == ZkTxType::L1 {
            // L1 contracts enforce a higher minimal limit for extra security
            gas_used = gas_used.max(L1_TX_MINIMAL_GAS_LIMIT);
            lowest_gas_limit = lowest_gas_limit.max(L1_TX_MINIMAL_GAS_LIMIT);
            highest_gas_limit = highest_gas_limit.max(L1_TX_MINIMAL_GAS_LIMIT);
        }

        // Pick a point that's close to the estimated gas
        let mut mid_gas_limit = std::cmp::min(
            gas_used * 3,
            ((highest_gas_limit as u128 + lowest_gas_limit as u128) / 2) as u64,
        );

        // Binary search narrows the range to find the minimum gas limit needed for the transaction
        // to succeed.
        while lowest_gas_limit + 1 < highest_gas_limit {
            // An estimation error is allowed once the current gas limit range used in the binary
            // search is small enough (less than 1.5% of the highest gas limit)
            // <https://github.com/ethereum/go-ethereum/blob/a5a4fa7032bb248f5a7c40f4e8df2b131c4186a4/eth/gasestimator/gasestimator.go#L152
            if (highest_gas_limit - lowest_gas_limit) as f64 / (highest_gas_limit as f64)
                < ESTIMATE_GAS_ERROR_RATIO
            {
                break;
            };

            let mut mid_tx = tx.clone();
            set_gas_limit(&mut mid_tx, mid_gas_limit);
            tracing::trace!(
                gas_limit = mid_tx.gas_limit(),
                "trying to simulate transaction"
            );

            // Execute transaction and handle potential gas errors, adjusting limits accordingly.
            match execute(mid_tx, block_context, storage_view.clone())
                .map_err(EthCallError::ForwardSubsystemError)?
            {
                Err(InvalidTransaction::CallerGasLimitMoreThanBlock) => {
                    // Decrease the highest gas limit if gas is too high
                    highest_gas_limit = mid_gas_limit;
                }
                Err(
                    InvalidTransaction::CallGasCostMoreThanGasLimit
                    | InvalidTransaction::OutOfGasDuringValidation
                    | InvalidTransaction::OutOfNativeResourcesDuringValidation,
                ) => {
                    // Increase the lowest gas limit if gas is too low
                    lowest_gas_limit = mid_gas_limit;
                }
                // Handle other cases, including successful transactions.
                ethres => {
                    // Unpack the result and environment if the transaction was successful.
                    res = ethres.map_err(EthCallError::InvalidTransaction)?;
                    // Update the estimated gas range based on the transaction result.
                    update_estimated_gas_range(
                        res.execution_result,
                        mid_gas_limit,
                        &mut highest_gas_limit,
                        &mut lowest_gas_limit,
                    )?;
                }
            }

            // New midpoint
            mid_gas_limit = ((highest_gas_limit as u128 + lowest_gas_limit as u128) / 2) as u64;
        }

        Ok(U256::from(highest_gas_limit))
    }
}

fn set_gas_limit(tx: &mut ZkTransaction, gas_limit: u64) {
    match tx.inner.inner_mut() {
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

#[inline]
pub fn update_estimated_gas_range(
    result: ExecutionResult,
    tx_gas_limit: u64,
    highest_gas_limit: &mut u64,
    lowest_gas_limit: &mut u64,
) -> Result<(), EthCallError> {
    match result {
        ExecutionResult::Success { .. } => {
            // Cap the highest gas limit with the succeeding gas limit.
            *highest_gas_limit = tx_gas_limit;
        }
        ExecutionResult::Revert { .. } => {
            // We know that transaction succeeded with a higher gas limit before, so any failure
            // means that we need to increase it.
            //
            // We are ignoring all halts here, and not just OOG errors because there are cases when
            // non-OOG halt might flag insufficient gas limit as well.
            //
            // Common usage of invalid opcode in OpenZeppelin:
            // <https://github.com/OpenZeppelin/openzeppelin-contracts/blob/94697be8a3f0dfcd95dfb13ffbd39b5973f5c65d/contracts/metatx/ERC2771Forwarder.sol#L360-L367>
            *lowest_gas_limit = tx_gas_limit;
        }
    };

    Ok(())
}

/// Error types returned by `eth_call` implementation
#[derive(Debug, thiserror::Error)]
pub enum EthCallError {
    // todo: temporary, needs to be supported eventually
    #[error("block overrides are not supported in `eth_call`")]
    BlockOverridesNotSupported,
    // todo(EIP-4844)
    #[error("EIP-4844 transactions are not supported")]
    Eip4844NotSupported,
    // todo(EIP-7702)
    #[error("EIP-7702 transactions are not supported")]
    Eip7702NotSupported,
    #[error("upgrade transactions cannot be estimated")]
    UpgradeTxNotEstimatable,

    /// Block could not be found by its id (hash/number/tag).
    #[error("block not found")]
    BlockNotFound(BlockId),

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
    Repository(#[from] RepositoryError),
    #[error(transparent)]
    State(#[from] StateError),
}
