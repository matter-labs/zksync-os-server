use anyhow::Context;
use zksync_types::api::BlockIdVariant;
use zksync_types::api::state_override::StateOverride;
use zksync_types::l2::L2Tx;
use zksync_types::PackedEthSignature;
use zksync_types::transaction_request::CallRequest;
use zksync_types::web3::Bytes;
use zksync_os_state::StateHandle;
use crate::api::resolve_block_id;
use crate::block_replay_storage::BlockReplayStorage;
use crate::config::RpcConfig;
use crate::execution::sandbox::execute;
use crate::finality::FinalityTracker;

pub struct EthCallHandler {
    config: RpcConfig,
    finality_info: FinalityTracker,
    state_handle: StateHandle,

    block_replay_storage: BlockReplayStorage
}

impl EthCallHandler {
    pub fn new(
        config: RpcConfig,
        finality_tracker: FinalityTracker,
        state_handle: StateHandle,
        block_replay_storage: BlockReplayStorage
    ) -> EthCallHandler {
        Self {
            config,
            finality_info: finality_tracker,
            state_handle,
            block_replay_storage
        }
    }

    pub fn call_impl(
        &self,
        mut req: CallRequest,
        block: Option<BlockIdVariant>,
        state_override: Option<StateOverride>,
    ) -> anyhow::Result<Bytes> {
        anyhow::ensure!(state_override.is_none());

        if req.gas.is_none() {
            req.gas = Some(self.config.eth_call_gas.into());
        }

        // todo Daniyar
        // todo: doing `+ 1` to make sure eth_calls are done on top of the current chain
        // consider legacy logic - perhaps differentiate Latest/Commiteed etc
        let block_number = resolve_block_id(block, &self.finality_info) + 1;
        tracing::info!("block {:?} resolved to: {:?}", block, block_number);

        let mut tx = L2Tx::from_request(req.clone().into(), self.config.max_tx_size_bytes, true)?;

        // otherwise it's not parsed properly in VM
        if tx.common_data.signature.is_empty() {
            tx.common_data.signature = PackedEthSignature::default().serialize_packed().into();
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

        Ok(res.as_returned_bytes().into())
    }
}