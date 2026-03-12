//! This module provides a unified interface for running blocks and simulating transactions.
//!
//! New execution versions should implement [`zksync_os_plugin_api::ExecutionPlugin`] in their own
//! crate (see `plugin-v6` for an example) and be registered in this module's dispatch functions.
//!
//! Legacy versions (V1–V5) are frozen and dispatched directly to their respective
//! `forward_system` crates without going through the plugin trait.

use zk_os_forward_system::run::RunBlockForward as RunBlockForwardV5Running;
use zk_os_forward_system_0_0_28::run::RunBlockForward as RunBlockForwardV3;
use zk_os_forward_system_0_1_2::run::RunBlockForward as RunBlockForwardV4;
use zk_os_forward_system_0_2_8::run::RunBlockForward as RunBlockForwardV5Simulation;
use zksync_os_interface::error::InvalidTransaction;
use zksync_os_interface::tracing::{AnyTracer, NopValidator};
use zksync_os_interface::traits::{
    EncodedTx, PreimageSource, ReadStorage, RunBlock, SimulateTx, TxResultCallback, TxSource,
};
use zksync_os_interface::types::BlockContext;
use zksync_os_interface::types::{BlockOutput, TxOutput};
use zksync_os_plugin_api::ExecutionPlugin;

mod adapter;
pub mod apps;

pub use adapter::AbiTxSource;
use zksync_os_types::ExecutionVersion;

static PLUGIN_V6: zksync_os_plugin_v6::PluginV6 = zksync_os_plugin_v6::PluginV6;

pub fn run_block<
    Storage: ReadStorage,
    PreimgSrc: PreimageSource,
    TrSrc: TxSource,
    TrCallback: TxResultCallback,
    Tracer: AnyTracer,
>(
    block_context: BlockContext,
    storage: Storage,
    preimage_source: PreimgSrc,
    tx_source: TrSrc,
    tx_result_callback: TrCallback,
    tracer: &mut Tracer,
) -> Result<BlockOutput, anyhow::Error> {
    let execution_version: ExecutionVersion = block_context
        .execution_version
        .try_into()
        .expect("Unsupported ZKsync OS execution version");
    match execution_version {
        // Legacy versions — frozen, dispatched directly to their forward_system crates.
        ExecutionVersion::V1 | ExecutionVersion::V2 | ExecutionVersion::V3 => {
            let object = RunBlockForwardV3 {};
            object
                .run_block(
                    (),
                    block_context,
                    storage,
                    preimage_source,
                    AbiTxSource::new(tx_source),
                    tx_result_callback,
                    tracer,
                    &mut NopValidator,
                )
                .map_err(|err| anyhow::anyhow!(err))
        }
        ExecutionVersion::V4 => {
            let object = RunBlockForwardV4 {};
            object
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
        ExecutionVersion::V5 => {
            // We use two different versions of zksync-os for execution and simulation:
            // * v0.2.5 is used to forward-run and prove blocks
            // * v0.2.6-simulation-only is used for simulation
            //
            // This is needed so that `eth_estimateGas` can work with 0-balance accounts. The fix was
            // not a part of v0.2.5 and unfortunately cannot be included without changing `app.bin`.
            let object = RunBlockForwardV5Running {};
            object
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
        // V6+ — dispatched through the ExecutionPlugin trait.
        ExecutionVersion::V6 => PLUGIN_V6.run_block(
            block_context,
            storage,
            preimage_source,
            tx_source,
            tx_result_callback,
            tracer,
        ),
    }
}

pub fn simulate_tx<Storage: ReadStorage, PreimgSrc: PreimageSource, Tracer: AnyTracer>(
    transaction: EncodedTx,
    block_context: BlockContext,
    storage: Storage,
    preimage_source: PreimgSrc,
    tracer: &mut Tracer,
) -> Result<Result<TxOutput, InvalidTransaction>, anyhow::Error> {
    let execution_version: ExecutionVersion = block_context
        .execution_version
        .try_into()
        .expect("Unsupported ZKsync OS execution version");
    match execution_version {
        // Legacy versions — frozen, dispatched directly to their forward_system crates.
        ExecutionVersion::V1 | ExecutionVersion::V2 | ExecutionVersion::V3 => {
            let object = RunBlockForwardV3 {};
            object
                .simulate_tx(
                    (),
                    adapter::convert_tx_to_abi(transaction),
                    block_context,
                    storage,
                    preimage_source,
                    tracer,
                    &mut NopValidator,
                )
                .map_err(|err| anyhow::anyhow!(err))
        }
        ExecutionVersion::V4 => {
            let object = RunBlockForwardV4 {};
            object
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
        ExecutionVersion::V5 => {
            // We use two different versions of zksync-os for execution and simulation:
            // * v0.2.5 is used to forward-run and prove blocks
            // * v0.2.6-simulation-only is used for simulation
            //
            // This is needed so that `eth_estimateGas` can work with 0-balance accounts. The fix was
            // not a part of v0.2.5 and unfortunately cannot be included without changing `app.bin`.
            let object = RunBlockForwardV5Simulation {};
            object
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
        // V6+ — dispatched through the ExecutionPlugin trait.
        ExecutionVersion::V6 => {
            PLUGIN_V6.simulate_tx(transaction, block_context, storage, preimage_source, tracer)
        }
    }
}
