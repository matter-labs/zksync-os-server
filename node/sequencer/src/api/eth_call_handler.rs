use crate::api::resolve_block_id;
use crate::block_replay_storage::BlockReplayStorage;
use crate::config::RpcConfig;
use crate::execution::sandbox::execute;
use crate::finality::FinalityTracker;
use alloy::eips::BlockId;
use alloy::primitives::Bytes;
use alloy::rpc::types::{BlockOverrides, TransactionRequest};
use anyhow::Context;
use zksync_os_state::StateHandle;

pub struct EthCallHandler {
    config: RpcConfig,
    finality_info: FinalityTracker,
    state_handle: StateHandle,

    block_replay_storage: BlockReplayStorage,
}

impl EthCallHandler {
    pub fn new(
        config: RpcConfig,
        finality_tracker: FinalityTracker,
        state_handle: StateHandle,
        block_replay_storage: BlockReplayStorage,
    ) -> EthCallHandler {
        Self {
            config,
            finality_info: finality_tracker,
            state_handle,
            block_replay_storage,
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

        if request.gas.is_none() {
            request.gas = Some(self.config.eth_call_gas as u64);
        }

        // todo Daniyar
        // todo: doing `+ 1` to make sure eth_calls are done on top of the current chain
        // consider legacy logic - perhaps differentiate Latest/Commiteed etc
        let block_number = resolve_block_id(block, &self.finality_info) + 1;
        tracing::info!("block {:?} resolved to: {:?}", block, block_number);

        // TODO: Double-check that this is an accurate method to use here
        let tx = request.build_typed_simulate_transaction()?;
        if tx.eip2718_encoded_length() * 32 > self.config.max_tx_size_bytes {
            anyhow::bail!(
                "oversized data. max: {}; actual: {}",
                self.config.max_tx_size_bytes,
                tx.eip2718_encoded_length() * 32
            );
        }

        // using previous block context
        let block_context = self
            .block_replay_storage
            // todo: why `block_number - 1`??
            .get_replay_record(block_number - 1)
            .context("Failed to get block context")?
            .context;

        let storage_view = self.state_handle.state_view_at_block(block_number)?;

        let res = execute(tx, block_context, storage_view)?;

        Ok(Bytes::copy_from_slice(res.as_returned_bytes()))
    }
}
