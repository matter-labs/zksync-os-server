//! This module provides a unified interface for running blocks and simulating transactions.
//! When adding a new VM version, make sure it is handled in `run_block` and `simulate_tx` methods.

use zk_os_forward_system::run::RunBlockForward as RunBlockForwardV5Running;
use zk_os_forward_system_0_0_28::run::RunBlockForward as RunBlockForwardV3;
use zk_os_forward_system_0_1_2::run::RunBlockForward as RunBlockForwardV4;
use zk_os_forward_system_0_2_8::run::RunBlockForward as RunBlockForwardV5Simulation;
use zk_os_forward_system_dev::run::RunBlockForward as RunBlockForwardV6;
use zksync_os_interface::error::InvalidTransaction;
use zksync_os_interface::tracing::{AnyTracer, NopValidator};
use zksync_os_interface::traits::{
    EncodedTx, PreimageSource, ReadStorage, RunBlock, SimulateTx, TxResultCallback, TxSource,
};
use zksync_os_interface::types::BlockContext;
use zksync_os_interface::types::{BlockOutput, TxOutput};

mod adapter;
pub mod apps;

pub use adapter::AbiTxSource;

/// VM version constants. These correspond to the `execution_version` field in `BlockContext`,
/// derived from the protocol version's minor number via `ProtocolSemanticVersion::vm_version()`.
const VM_V3: u32 = 3;
const VM_V4: u32 = 4;
const VM_V5: u32 = 5;
const VM_V6: u32 = 6;

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
    match block_context.execution_version {
        1..=VM_V3 => {
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
        VM_V4 => {
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
        VM_V5 => {
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
        VM_V6 => {
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
                    &mut NopValidator,
                )
                .map_err(|err| anyhow::anyhow!(err))
        }
        other => panic!("Unsupported VM version: {other}"),
    }
}

pub fn simulate_tx<Storage: ReadStorage, PreimgSrc: PreimageSource, Tracer: AnyTracer>(
    transaction: EncodedTx,
    block_context: BlockContext,
    storage: Storage,
    preimage_source: PreimgSrc,
    tracer: &mut Tracer,
) -> Result<Result<TxOutput, InvalidTransaction>, anyhow::Error> {
    match block_context.execution_version {
        1..=VM_V3 => {
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
        VM_V4 => {
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
        VM_V5 => {
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
        VM_V6 => {
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
        other => panic!("Unsupported VM version for simulation: {other}"),
    }
}
