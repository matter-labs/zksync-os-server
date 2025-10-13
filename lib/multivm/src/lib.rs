//! This module provides a unified interface for running blocks and simulating transactions.
//! When adding new ZKsync OS execution version, make sure it is handled in `run_block` and `simulate_tx` methods.
//! Also, update the `LATEST_EXECUTION_VERSION` constant accordingly.

use num_enum::TryFromPrimitive;
use zk_os_forward_system::run::RunBlockForward as RunBlockForwardV3;
use zksync_os_interface::error::InvalidTransaction;
use zksync_os_interface::tracing::AnyTracer;
use zksync_os_interface::traits::{
    PreimageSource, ReadStorage, RunBlock, SimulateTx, TxResultCallback, TxSource,
};
use zksync_os_interface::types::BlockContext;
use zksync_os_interface::types::{BlockOutput, TxOutput};

pub mod apps;

#[derive(Debug, Clone, Copy, TryFromPrimitive)]
#[repr(u32)]
pub enum ExecutionVersion {
    V1 = 1,
    V2 = 2,
    V3 = 3,
}

pub const LATEST_EXECUTION_VERSION: ExecutionVersion = ExecutionVersion::V3;

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
        ExecutionVersion::V1 | ExecutionVersion::V2 | ExecutionVersion::V3 => {
            let object = RunBlockForwardV3 {};
            object
                .run_block(
                    (),
                    block_context,
                    storage,
                    preimage_source,
                    tx_source,
                    tx_result_callback,
                    tracer,
                )
                .map_err(|err| anyhow::anyhow!(err))
        }
    }
}

pub fn simulate_tx<Storage: ReadStorage, PreimgSrc: PreimageSource, Tracer: AnyTracer>(
    transaction: Vec<u8>,
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
        ExecutionVersion::V1 | ExecutionVersion::V2 | ExecutionVersion::V3 => {
            let object = RunBlockForwardV3 {};
            object
                .simulate_tx(
                    (),
                    transaction,
                    block_context,
                    storage,
                    preimage_source,
                    tracer,
                )
                .map_err(|err| anyhow::anyhow!(err))
        }
    }
}

pub fn proving_run_execution_version(forward_run_execution_version: u32) -> ExecutionVersion {
    let forward_run_execution_version: ExecutionVersion = forward_run_execution_version
        .try_into()
        .expect("Unsupported ZKsync OS execution version");
    match forward_run_execution_version {
        ExecutionVersion::V1 | ExecutionVersion::V2 | ExecutionVersion::V3 => ExecutionVersion::V3,
    }
}
