//! Execution plugin for zksync-os V6 (dev version).
//!
//! Wraps `zk_os_forward_system_dev` behind the stable [`ExecutionPlugin`] trait.

use zk_os_forward_system_dev::run::RunBlockForward;
use zksync_os_interface::error::InvalidTransaction;
use zksync_os_interface::tracing::{AnyTracer, NopValidator};
use zksync_os_interface::traits::{
    EncodedTx, PreimageSource, ReadStorage, RunBlock, SimulateTx, TxResultCallback, TxSource,
};
use zksync_os_interface::types::{BlockContext, BlockOutput, TxOutput};
use zksync_os_plugin_api::ExecutionPlugin;

pub struct PluginV6;

impl ExecutionPlugin for PluginV6 {
    fn run_block<S, P, T, C, Tr>(
        &self,
        block_context: BlockContext,
        storage: S,
        preimage_source: P,
        tx_source: T,
        tx_result_callback: C,
        tracer: &mut Tr,
    ) -> Result<BlockOutput, anyhow::Error>
    where
        S: ReadStorage,
        P: PreimageSource,
        T: TxSource,
        C: TxResultCallback,
        Tr: AnyTracer,
    {
        RunBlockForward {}
            .run_block(
                (),
                block_context,
                storage,
                preimage_source,
                tx_source,
                tx_result_callback,
                tracer,
                &mut NopValidator,
            )
            .map_err(|err| anyhow::anyhow!(err))
    }

    fn simulate_tx<S, P, Tr>(
        &self,
        transaction: EncodedTx,
        block_context: BlockContext,
        storage: S,
        preimage_source: P,
        tracer: &mut Tr,
    ) -> Result<Result<TxOutput, InvalidTransaction>, anyhow::Error>
    where
        S: ReadStorage,
        P: PreimageSource,
        Tr: AnyTracer,
    {
        RunBlockForward {}
            .simulate_tx(
                (),
                transaction,
                block_context,
                storage,
                preimage_source,
                tracer,
                &mut NopValidator,
            )
            .map_err(|err| anyhow::anyhow!(err))
    }
}
