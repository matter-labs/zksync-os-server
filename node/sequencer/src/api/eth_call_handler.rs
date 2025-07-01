use crate::api::resolve_block_id;
use crate::block_replay_storage::BlockReplayStorage;
use crate::config::RpcConfig;
use crate::execution::sandbox::execute;
use crate::finality::FinalityTracker;
use crate::repositories::AccountPropertyRepository;
use alloy::eips::BlockId;
use alloy::network::TransactionBuilder;
use alloy::primitives::Bytes;
use alloy::rpc::types::{BlockOverrides, TransactionRequest};
use anyhow::Context;
use std::cmp::min;
use zksync_os_state::StateHandle;

pub struct EthCallHandler {
    config: RpcConfig,
    finality_info: FinalityTracker,
    state_handle: StateHandle,

    block_replay_storage: BlockReplayStorage,
    account_property_repository: AccountPropertyRepository,
}

impl EthCallHandler {
    pub fn new(
        config: RpcConfig,
        finality_tracker: FinalityTracker,
        state_handle: StateHandle,
        block_replay_storage: BlockReplayStorage,
        account_property_repository: AccountPropertyRepository,
    ) -> EthCallHandler {
        Self {
            config,
            finality_info: finality_tracker,
            state_handle,
            block_replay_storage,
            account_property_repository,
        }
    }

    pub fn call_impl(
        &self,
        mut request: TransactionRequest,
        block: Option<BlockId>,
        state_overrides: Option<alloy::rpc::types::state::StateOverride>,
        block_overrides: Option<Box<BlockOverrides>>,
    ) -> anyhow::Result<Bytes> {
        anyhow::ensure!(state_overrides.is_none());
        anyhow::ensure!(block_overrides.is_none());

        // todo Daniyar
        // todo: doing `+ 1` to make sure eth_calls are done on top of the current chain
        // consider legacy logic - perhaps differentiate Latest/Commiteed etc
        let block_number = resolve_block_id(block, &self.finality_info) + 1;
        tracing::info!("block {:?} resolved to: {:?}", block, block_number);

        // using previous block context
        let block_context = self
            .block_replay_storage
            // todo: why `block_number - 1`??
            .get_replay_record(block_number - 1)
            .context("Failed to get block context")?
            .context;

        let tx_type = request.minimal_tx_type();

        if request.gas.is_none() {
            request.gas = Some(self.config.eth_call_gas as u64);
        }

        if request.nonce.is_none() {
            let nonce = self
                .account_property_repository
                .get_latest(&request.from.unwrap_or_default())
                .map(|props| props.nonce)
                .unwrap_or_default();
            request.nonce.replace(nonce);
        }

        let call_gas_price = request.gas_price;
        let call_max_fee_per_gas = request.max_fee_per_gas;
        let call_max_priority_fee_per_gas = request.max_priority_fee_per_gas;
        let block_base_fee = u128::try_from(block_context.eip1559_basefee)
            .expect("block base fee is not a valid u128");
        match (
            call_gas_price,
            call_max_fee_per_gas,
            call_max_priority_fee_per_gas,
        ) {
            (gas_price, None, None) => {
                // either legacy transaction or no fee fields are specified
                // when no fields are specified, set gas price to zero
                request.gas_price.replace(gas_price.unwrap_or_default());
            }
            (None, max_fee_per_gas, max_priority_fee_per_gas) => match max_fee_per_gas {
                Some(max_fee_per_gas) => {
                    let max_priority_fee_per_gas = max_priority_fee_per_gas.unwrap_or_default();

                    // only enforce the fee cap if provided input is not zero
                    if !(max_fee_per_gas == 0 && max_priority_fee_per_gas == 0)
                        && max_fee_per_gas < block_base_fee
                    {
                        anyhow::bail!("`maxFeePerGas` less than `block.baseFee`");
                    }
                    if max_fee_per_gas < max_priority_fee_per_gas {
                        anyhow::bail!("`maxPriorityFeePerGas` higher than `maxFeePerGas`")
                    }
                    request.max_fee_per_gas.replace(min(
                        max_fee_per_gas,
                        block_base_fee
                            .checked_add(max_priority_fee_per_gas)
                            .ok_or(anyhow::anyhow!("`maxPriorityFeePerGas` is too high"))?,
                    ));
                }
                None => {
                    request.max_fee_per_gas.replace(
                        block_base_fee
                            .checked_add(max_priority_fee_per_gas.unwrap_or_default())
                            .ok_or(anyhow::anyhow!("`maxPriorityFeePerGas` is too high"))?,
                    );
                }
            },
            // TODO: Handle EIP-4844 fees properly
            _ => {
                anyhow::bail!(
                    "found both `gasPrice` and (`maxFeePerGas` or `maxPriorityFeePerGas`)"
                )
            }
        }

        // TODO: populate other `request` fields (`chain_id`, `from` etc)

        if let Err(errs) = request.complete_type(tx_type) {
            for err in errs {
                tracing::error!(err, "missing field")
            }
        }
        let tx = request.build_typed_simulate_transaction()?;
        if tx.eip2718_encoded_length() * 32 > self.config.max_tx_size_bytes {
            anyhow::bail!(
                "oversized data. max: {}; actual: {}",
                self.config.max_tx_size_bytes,
                tx.eip2718_encoded_length() * 32
            );
        }

        let storage_view = self.state_handle.state_view_at_block(block_number)?;

        let res = execute(tx, block_context, storage_view)?;

        Ok(Bytes::copy_from_slice(res.as_returned_bytes()))
    }
}
