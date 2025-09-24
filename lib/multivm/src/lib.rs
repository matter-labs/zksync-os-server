//! This module provides a unified interface for running blocks and simulating transactions.
//! When adding new ZKsync OS execution version, make sure it is handled in `run_block` and `simulate_tx` methods.
//! Also, update the `LATEST_EXECUTION_VERSION` constant accordingly.

use alloy::primitives::B256;
use zk_os_forward_system::run::RunBlockForward as RunBlockForwardV2;
use zk_os_forward_system_0_0_23::run::RunBlockForward as RunBlockForwardV1;
use zksync_os_interface::error::InvalidTransaction;
use zksync_os_interface::traits::{
    PreimageSource, ReadStorage, RunBlock, SimulateTx, TxResultCallback, TxSource,
};
use zksync_os_interface::types::BlockContext;
use zksync_os_interface::types::{BlockOutput, TxOutput};

pub mod apps;

pub fn run_block<S: ReadStorage, PS: PreimageSource, TS: TxSource, TR: TxResultCallback>(
    block_context: BlockContext,
    storage: S,
    preimage_source: PS,
    tx_source: TS,
    tx_result_callback: TR,
) -> Result<BlockOutput, anyhow::Error> {
    match block_context.execution_version {
        1 => {
            let object = RunBlockForwardV1 {};
            object
                .run_block(
                    (),
                    block_context,
                    storage,
                    preimage_source,
                    tx_source,
                    tx_result_callback,
                )
                .map_err(|err| anyhow::anyhow!(err))
        }
        2 => {
            let object = RunBlockForwardV2 {};
            object
                .run_block(
                    (),
                    block_context,
                    storage,
                    preimage_source,
                    tx_source,
                    tx_result_callback,
                )
                .map_err(|err| anyhow::anyhow!(err))
        }
        v => panic!("Unsupported ZKsync OS execution version: {v}"),
    }
}

pub fn simulate_tx<S: ReadStorage, PS: PreimageSource>(
    transaction: Vec<u8>,
    block_context: BlockContext,
    storage: S,
    preimage_source: PS,
) -> Result<Result<TxOutput, InvalidTransaction>, anyhow::Error> {
    match block_context.execution_version {
        1 => {
            let object = RunBlockForwardV1 {};
            object
                .simulate_tx((), transaction, block_context, storage, preimage_source)
                .map_err(|err| anyhow::anyhow!(err))
        }
        2 => {
            let object = RunBlockForwardV2 {};
            object
                .simulate_tx((), transaction, block_context, storage, preimage_source)
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

pub fn vk_for_execution_version(forward_run_execution_version: u32) -> B256 {
    match proving_run_execution_version(forward_run_execution_version) {
        1 => apps::v1::VERIFICATION_KEY,
        2 => apps::v2::VERIFICATION_KEY,
        v => panic!("Unsupported ZKsync OS execution version: {v}"),
    }
}
