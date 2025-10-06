//! This module provides a unified interface for running blocks and simulating transactions.
//! When adding new ZKsync OS execution version, make sure it is handled in `run_block` and `simulate_tx` methods.
//! Also, update the `LATEST_EXECUTION_VERSION` constant accordingly.

use zk_os_forward_system::run::RunBlockForward as RunBlockForwardV2;
use zksync_os_interface::error::InvalidTransaction;
use zksync_os_interface::tracing::{AnyTracer, EvmTracer};
use zksync_os_interface::traits::{
    PreimageSource, ReadStorage, RunBlock, SimulateTx, TxResultCallback, TxSource,
};
use zksync_os_interface::types::BlockContext;
use zksync_os_interface::types::{BlockOutput, TxOutput};

pub mod apps;

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
        1 | 2 => {
            let object = RunBlockForwardV2 {};
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
        v => panic!("Unsupported ZKsync OS execution version: {v}"),
    }
}

pub fn simulate_tx<Storage: ReadStorage, PreimgSrc: PreimageSource, Tracer: EvmTracer>(
    transaction: Vec<u8>,
    block_context: BlockContext,
    storage: Storage,
    preimage_source: PreimgSrc,
    tracer: &mut Tracer,
) -> Result<Result<TxOutput, InvalidTransaction>, anyhow::Error> {
    match block_context.execution_version {
        1 | 2 => {
            let object = RunBlockForwardV2 {};
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
        v => panic!("Unsupported ZKsync OS execution version: {v}"),
    }
}

pub const LATEST_EXECUTION_VERSION: u32 = 2;

pub fn proving_run_execution_version(forward_run_execution_version: u32) -> u32 {
    match forward_run_execution_version {
        1 | 2 => 2,
        v => panic!("Unsupported ZKsync OS execution version: {v}"),
    }
}
