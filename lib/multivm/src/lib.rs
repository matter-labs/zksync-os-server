//! This module provides a unified interface for running blocks and simulating transactions.
//! When adding new ZKsync OS execution version, make sure it is handled in `run_block` and `simulate_tx` methods.

use zk_os_forward_system::run::RunBlockForward as RunBlockForwardV5Running;
use zk_os_forward_system_dev::run::RunBlockForward as RunBlockForwardV6;
use zk_os_forward_system_v3::run::RunBlockForward as RunBlockForwardV3;
use zk_os_forward_system_v4::run::RunBlockForward as RunBlockForwardV4;
use zk_os_forward_system_v5_simulation::run::RunBlockForward as RunBlockForwardV5Simulation;
use zksync_os_interface::error::InvalidTransaction;
use zksync_os_interface::tracing::{AnyTracer, AnyTxValidator, NopValidator};
use zksync_os_interface::traits::{
    EncodedTx, PreimageSource, ReadStorage, RunBlock, SimulateTx, TxResultCallback, TxSource,
};
use zksync_os_interface::types::BlockContext;
use zksync_os_interface::types::{BlockOutput, TxOutput};
use zksync_os_types::protocol_config::ForwardSystemVersion;

mod adapter;
pub mod apps;

pub use adapter::AbiTxSource;

pub fn run_block<
    Storage: ReadStorage,
    PreimgSrc: PreimageSource,
    TrSrc: TxSource,
    TrCallback: TxResultCallback,
    Tracer: AnyTracer,
    Validator: AnyTxValidator,
>(
    block_context: BlockContext,
    storage: Storage,
    preimage_source: PreimgSrc,
    tx_source: TrSrc,
    tx_result_callback: TrCallback,
    tracer: &mut Tracer,
    validator: &mut Validator,
) -> Result<BlockOutput, anyhow::Error> {
    let version = ForwardSystemVersion::try_from(block_context.execution_version)
        .unwrap_or_else(|v| panic!("Unsupported ZKsync OS execution version: {v}"));
    match version {
        ForwardSystemVersion::V1 | ForwardSystemVersion::V2 | ForwardSystemVersion::V3 => {
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
                    validator,
                )
                .map_err(|err| anyhow::anyhow!(err))
        }
        ForwardSystemVersion::V4 => {
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
                    validator,
                )
                .map_err(|err| anyhow::anyhow!(err))
        }
        ForwardSystemVersion::V5 => {
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
                    validator,
                )
                .map_err(|err| anyhow::anyhow!(err))
        }
        ForwardSystemVersion::V6 => {
            let object = RunBlockForwardV6 {};
            object
                .run_block(
                    (),
                    block_context,
                    storage,
                    preimage_source,
                    tx_source,
                    tx_result_callback,
                    tracer,
                    validator,
                )
                .map_err(|err| anyhow::anyhow!(err))
        }
    }
}

pub fn simulate_tx<Storage: ReadStorage, PreimgSrc: PreimageSource, Tracer: AnyTracer>(
    transaction: EncodedTx,
    block_context: BlockContext,
    storage: Storage,
    preimage_source: PreimgSrc,
    tracer: &mut Tracer,
) -> Result<Result<TxOutput, InvalidTransaction>, anyhow::Error> {
    let version = ForwardSystemVersion::try_from(block_context.execution_version)
        .unwrap_or_else(|v| panic!("Unsupported ZKsync OS execution version: {v}"));
    match version {
        ForwardSystemVersion::V1 | ForwardSystemVersion::V2 | ForwardSystemVersion::V3 => {
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
        ForwardSystemVersion::V4 => {
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
        ForwardSystemVersion::V5 => {
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
        ForwardSystemVersion::V6 => {
            let object = RunBlockForwardV6 {};
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
    }
}
