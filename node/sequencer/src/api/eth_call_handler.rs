use crate::api::call_fees::{CallFees, CallFeesError};
use crate::block_replay_storage::BlockReplayStorage;
use crate::config::RpcConfig;
use crate::execution::sandbox::execute;
use crate::repositories::api_interface::{ApiRepository, ApiRepositoryExt, RepositoryError};
use alloy::consensus::transaction::Recovered;
use alloy::consensus::{SignableTransaction, TxEip1559, TxEip2930, TxLegacy, TxType};
use alloy::eips::BlockId;
use alloy::primitives::{BlockNumber, Bytes, Signature, TxKind};
use alloy::rpc::types::{BlockOverrides, TransactionRequest};
use zk_os_forward_system::run::errors::ForwardSubsystemError;
use zk_os_forward_system::run::InvalidTransaction;
use zksync_os_state::StateHandle;
use zksync_os_types::L2Envelope;

pub struct EthCallHandler<R> {
    config: RpcConfig,
    state_handle: StateHandle,

    block_replay_storage: BlockReplayStorage,
    repository: R,
}

impl<R: ApiRepository> EthCallHandler<R> {
    pub fn new(
        config: RpcConfig,
        state_handle: StateHandle,
        block_replay_storage: BlockReplayStorage,
        repository: R,
    ) -> EthCallHandler<R> {
        Self {
            config,
            state_handle,
            block_replay_storage,
            repository,
        }
    }

    pub fn call_impl(
        &self,
        request: TransactionRequest,
        block: Option<BlockId>,
        state_overrides: Option<alloy::rpc::types::state::StateOverride>,
        block_overrides: Option<Box<BlockOverrides>>,
    ) -> Result<Bytes, EthCallError> {
        if state_overrides.is_some() {
            return Err(EthCallError::StateOverridesNotSupported);
        }
        if block_overrides.is_some() {
            return Err(EthCallError::BlockOverridesNotSupported);
        }

        let block_id = block.unwrap_or_default();
        let Some(block_number) = self.repository.resolve_block_number(block_id)? else {
            return Err(EthCallError::BlockNotFound(block_id));
        };
        tracing::info!(?block_id, block_number, "resolved block id");

        // using previous block context
        let block_context = self
            .block_replay_storage
            .get_replay_record(block_number)
            .ok_or(EthCallError::BlockNotFound(block_id))?
            .block_context;

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
        let nonce = nonce.unwrap_or_else(|| {
            self.repository
                .account_property_repository()
                .get_at_block(block_number, &from.unwrap_or_default())
                .map(|props| props.nonce)
                .unwrap_or_default()
        });

        let CallFees {
            max_priority_fee_per_gas,
            gas_price,
        } = CallFees::ensure_fees(
            gas_price,
            max_fee_per_gas,
            max_priority_fee_per_gas,
            block_context.eip1559_basefee.saturating_to(),
        )?;
        let chain_id = chain_id.unwrap_or(self.config.chain_id);
        let from = from.unwrap_or_default();
        let to = to.unwrap_or(TxKind::Create);
        let value = value.unwrap_or_default();
        let input = input.into_input().unwrap_or_default();

        // Mock signature as this is a simulated transaction
        let signature = Signature::new(Default::default(), Default::default(), false);
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
        let tx = Recovered::new_unchecked(tx, from);

        let storage_view = self
            .state_handle
            .state_view_at_block(block_number)
            // todo: introduce error hierarchy for `zksync_os_state`
            .map_err(|_| EthCallError::BlockStateNotAvailable(block_number))?;

        let res = execute(tx, block_context, storage_view)
            .map_err(EthCallError::ForwardSubsystemError)?
            .map_err(EthCallError::InvalidTransaction)?;

        Ok(Bytes::copy_from_slice(res.as_returned_bytes()))
    }
}

/// Error types returned by `eth_call` implementation
#[derive(Debug, thiserror::Error)]
pub enum EthCallError {
    // todo: temporary, needs to be supported eventually
    #[error("state overrides are not supported in `eth_call`")]
    StateOverridesNotSupported,
    // todo: temporary, needs to be supported eventually
    #[error("block overrides are not supported in `eth_call`")]
    BlockOverridesNotSupported,
    // todo(EIP-4844)
    #[error("EIP-4844 transactions are not supported")]
    Eip4844NotSupported,
    // todo(EIP-7702)
    #[error("EIP-7702 transactions are not supported")]
    Eip7702NotSupported,

    /// Block could not be found by its id (hash/number/tag).
    #[error("block not found")]
    BlockNotFound(BlockId),
    // todo: consider moving this to `zksync_os_state` crate
    /// Block has been compacted.
    #[error("state for block {0} is not available")]
    BlockStateNotAvailable(BlockNumber),

    /// Error while decoding or validating transaction request fees.
    #[error(transparent)]
    CallFees(#[from] CallFeesError),
    /// Missing a mandatary field `maxPriorityFeePerGas`. Only returned if transaction's minimal
    /// buildable type enforces this field to be present (i.e., not legacy or EIP-2930).
    #[error("missing `maxPriorityFeePerGas` field for EIP-1559 transaction")]
    MissingPriorityFee,

    // Below is more or less temporary as the error hierarchy in ZKsync OS is going through a major
    // refactoring.
    /// Internal error propagated by ZKsync OS.
    #[error("ZKsync OS error: {0:?}")]
    ForwardSubsystemError(ForwardSubsystemError),
    /// Transaction is invalid according to ZKsync OS.
    #[error("invalid transaction: {0:?}")]
    InvalidTransaction(InvalidTransaction),

    #[error(transparent)]
    RepositoryError(#[from] RepositoryError),
}
